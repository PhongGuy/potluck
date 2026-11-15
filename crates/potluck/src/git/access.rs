//! [`GitAccess`]: the seam that turns a git request's credential into a role on a workspace.

use crate::api::credential::Credential;
use crate::api::error::ApiError;
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
}

impl Access {
    /// Grants `role` with no hook environment.
    pub fn new(role: Role) -> Self {
        Self {
            role,
            hook_env: Vec::new(),
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
