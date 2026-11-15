//! [`GitAccess`]: the seam that turns a git request's credential into a role on a workspace.

use crate::api::Actor;
use crate::api::auth::authenticate_with;
use crate::api::credential::Credential;
use crate::api::error::ApiError;
use crate::api::workspaces::workspace_role;
use crate::db::Db;
use crate::role::Role;
use crate::slug::Slug;

/// A boxed future, so `GitAccess` is dyn-compatible without a macro crate.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// What a credential may do on `{team}/{ws}`, and what the pre-receive hook should know about
/// it. The git routes hand `hook_env` to the push as extra environment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Access {
    role: Role,
    hook_env: Vec<(String, String)>,
    push: Option<PushContext>,
}

impl Access {
    /// Grants `role` with no hook environment.
    pub fn new(role: Role) -> Self {
        Self {
            role,
            hook_env: Vec::new(),
            push: None,
        }
    }

    /// Adds one variable the hook will see.
    #[must_use]
    pub fn with_hook_env(mut self, key: &str, value: &str) -> Self {
        self.hook_env.push((key.to_owned(), value.to_owned()));
        self
    }

    /// The granted role.
    pub fn role(&self) -> Role {
        self.role
    }

    /// The variables for the hook, in insertion order.
    pub fn hook_env(&self) -> &[(String, String)] {
        &self.hook_env
    }

    /// Attaches the push context.
    #[must_use]
    pub fn with_push(mut self, push: PushContext) -> Self {
        self.push = Some(push);
        self
    }

    /// The push context, when the resolver attached one.
    pub fn push(&self) -> Option<&PushContext> {
        self.push.as_ref()
    }
}

/// What bookkeeping needs about an accepted push: the workspace row, who pushed, and the flags
/// the hook ran under. The resolver attaches it to a receive-pack `Access`; without it an
/// accepted push is served but not recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PushContext {
    workspace_id: i64,
    pusher: String,
    import: bool,
    override_id: Option<i64>,
}

impl PushContext {
    /// For `workspace_id`, pushed by `pusher` (a username), no flags.
    pub fn new(workspace_id: i64, pusher: &str) -> Self {
        Self {
            workspace_id,
            pusher: pusher.to_owned(),
            import: false,
            override_id: None,
        }
    }

    /// Whether the hook ran in import (tip-tree) mode.
    #[must_use]
    pub fn import(mut self, yes: bool) -> Self {
        self.import = yes;
        self
    }

    /// The override id the hook was given, if any.
    #[must_use]
    pub fn override_id(mut self, id: Option<i64>) -> Self {
        self.override_id = id;
        self
    }

    /// The workspace row id.
    pub fn workspace_id(&self) -> i64 {
        self.workspace_id
    }

    /// The pusher's username.
    pub fn pusher(&self) -> &str {
        &self.pusher
    }

    /// Whether the hook ran in import mode.
    pub fn is_import(&self) -> bool {
        self.import
    }

    /// The override id the hook was given, if any.
    pub fn armed_override(&self) -> Option<i64> {
        self.override_id
    }
}

/// Resolves a git request's credential to a role. `Err(unauthenticated)` for an unknown, revoked
/// or expired token; `Err(not_found)` for a workspace that does not exist or the caller cannot
/// read. The handler compares the role with the service and answers 403 itself.
pub trait GitAccess: Send + Sync + 'static {
    /// See the trait doc.
    fn resolve<'a>(
        &'a self,
        credential: &'a Credential,
        team: &'a Slug,
        ws: &'a Slug,
    ) -> BoxFuture<'a, Result<Access, ApiError>>;
}

/// The resolver until real token verification is wired: every token is unknown (401).
/// A plain unit struct, so callers can name it as a value.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAll;

impl GitAccess for DenyAll {
    fn resolve<'a>(
        &'a self,
        _credential: &'a Credential,
        _team: &'a Slug,
        _ws: &'a Slug,
    ) -> BoxFuture<'a, Result<Access, ApiError>> {
        Box::pin(async { Err(ApiError::unauthenticated("unknown token")) })
    }
}

/// The resolver over `server.db`: the credential through `authenticate_with` (401 for an
/// unknown, expired or revoked token or a disabled owner), the workspace by slugs (404), the
/// role through `workspace_role` (server admins are `Admin` everywhere, else the highest grant;
/// none is the same 404), and the hook environment, in this order: `POTLUCK_KIND` (the workspace's
/// kind), `POTLUCK_PUSHER` (username), `POTLUCK_PUSHER_TOKEN` (the token's row id),
/// `POTLUCK_IMPORT=1` only while the import state is set, `POTLUCK_OVERRIDE=<id>` only when an
/// override is armed for the pusher.
#[derive(Clone)]
#[non_exhaustive]
pub struct DbAccess {
    db: Db,
}

impl DbAccess {
    /// Over `db`.
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

impl GitAccess for DbAccess {
    fn resolve<'a>(
        &'a self,
        credential: &'a Credential,
        team: &'a Slug,
        ws: &'a Slug,
    ) -> BoxFuture<'a, Result<Access, ApiError>> {
        Box::pin(async move {
            let user = authenticate_with(&self.db, credential).await?;
            let actor = Actor::User(user.clone());
            // A workspace the caller holds no role on is answered exactly like a missing one.
            let no_ws = || ApiError::not_found("no such workspace");
            let Some(record) = self
                .db
                .workspace_by_slugs(team.as_str(), ws.as_str())
                .await?
            else {
                return Err(no_ws());
            };
            let Some(role) = workspace_role(&self.db, &actor, &record).await? else {
                return Err(no_ws());
            };
            // The flags are looked up only once the role is known, so a 404 costs no extra query.
            // The token travels by row id only; the secret itself never reaches the hook.
            let mut access = Access::new(role)
                .with_hook_env("POTLUCK_KIND", record.kind())
                .with_hook_env("POTLUCK_PUSHER", user.user().username())
                .with_hook_env("POTLUCK_PUSHER_TOKEN", &user.token_id().to_string());
            if record.import_pending() {
                access = access.with_hook_env("POTLUCK_IMPORT", "1");
            }
            let armed = self
                .db
                .armed_override(record.id(), user.user().id())
                .await?;
            if let Some(id) = armed {
                access = access.with_hook_env("POTLUCK_OVERRIDE", &id.to_string());
            }
            // The same values the hook was given, so bookkeeping consumes exactly that override.
            let push = PushContext::new(record.id(), user.user().username())
                .import(record.import_pending())
                .override_id(armed);
            Ok(access.with_push(push))
        })
    }
}
