//! The JSON API under `/api/v1`: shared state, the router and the error model.

use std::sync::Arc;

use axum::Router;
use axum::routing::{delete, get, patch, post, put};

use crate::auth::password::Passwords;
use crate::auth::rate_limit::SessionLimits;
use crate::config::Config;
use crate::db::Db;
use crate::git::access::{DbAccess, GitAccess};
use crate::git::{GitEngine, GitLimits, Repos};
use crate::metrics::Metrics;
use crate::notices::Notices;
use crate::oidc::{IdpHttp, OidcState, Provider, Timings};
use crate::update::UpdateChecker;

mod actor;
mod audit;
pub mod auth;
pub(crate) mod client_ip;
pub mod credential;
pub mod error;
mod events;
mod grants;
pub(crate) mod json;
pub mod objects;
mod oidc;
mod oidc_pages;
mod overrides;
pub(crate) mod pushes;
mod session;
mod system;
mod teams;
#[cfg(test)]
mod test_support;
mod tokens;
mod update;
mod users;
mod validate;
pub(crate) mod workspaces;

pub use actor::{Actor, AuthUser};
use error::{ApiError, ErrorResponse};

/// What every handler can reach: the database handle, the validated config, the git access
/// resolver, the git engine, the password hasher, the sign-in rate limiters, the metrics
/// registry, the OIDC state when a provider is configured and, when a release source is wired,
/// the update checker.
#[derive(Clone)]
#[non_exhaustive]
pub struct AppState {
    db: Db,
    config: Arc<Config>,
    git_access: Arc<dyn GitAccess>,
    git: Arc<GitEngine>,
    passwords: Passwords,
    session_limits: Arc<SessionLimits>,
    notices: Notices,
    updates: Option<UpdateChecker>,
    oidc: Option<OidcState>,
    metrics: Arc<Metrics>,
}

impl AppState {
    /// Bundles the handle and the config for the router, with the database-backed resolver,
    /// the default git limits under the configured run limit and the production hashing cost;
    /// the engine reads its paths and body cap from `config`.
    pub fn new(db: Db, config: Config) -> Self {
        let git = Arc::new(git_engine(
            &config,
            GitLimits::default().with_max_runtime(config.git_max_runtime()),
        ));
        let git_access = Arc::new(DbAccess::new(db.clone()));
        let oidc = oidc_state(&config);
        AppState {
            db,
            config: Arc::new(config),
            git_access,
            git,
            passwords: Passwords::new(),
            session_limits: Arc::new(SessionLimits::default()),
            notices: Notices::new(),
            updates: None,
            oidc,
            metrics: Metrics::new(env!("CARGO_PKG_VERSION")),
        }
    }

    /// Replaces the resolver (tests, and the real token-backed one).
    #[must_use]
    pub fn with_git_access(mut self, access: Arc<dyn GitAccess>) -> Self {
        self.git_access = access;
        self
    }

    /// Rebuilds the engine with `limits`.
    #[must_use]
    pub fn with_git_limits(mut self, limits: GitLimits) -> Self {
        self.git = Arc::new(git_engine(&self.config, limits));
        self
    }

    /// Another hashing cost (tests).
    #[must_use]
    pub fn with_passwords(mut self, passwords: Passwords) -> Self {
        self.passwords = passwords;
        self
    }

    /// Serves the update endpoints from `checker`; without one they report `check: "off"`.
    #[must_use]
    pub fn with_updates(mut self, checker: UpdateChecker) -> Self {
        self.updates = Some(checker);
        self
    }

    /// Replaces the OIDC state (tests, with a mock provider and short timings).
    #[must_use]
    pub fn with_oidc(mut self, oidc: OidcState) -> Self {
        self.oidc = Some(oidc);
        self
    }

    /// The OIDC state, when a provider is configured.
    pub fn oidc(&self) -> Option<&OidcState> {
        self.oidc.as_ref()
    }

    /// The update checker, when a release source is wired.
    pub(crate) fn updates(&self) -> Option<&UpdateChecker> {
        self.updates.as_ref()
    }

    /// The hasher.
    pub fn passwords(&self) -> &Passwords {
        &self.passwords
    }

    /// The `POST /session` limiters, shared by every clone of the state.
    pub fn session_limits(&self) -> &SessionLimits {
        &self.session_limits
    }

    /// The resolver.
    pub(crate) fn git_access(&self) -> &dyn GitAccess {
        &*self.git_access
    }

    /// The metrics registry, shared by every clone of the state.
    pub(crate) fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    /// The engine.
    pub(crate) fn git(&self) -> &GitEngine {
        &self.git
    }

    /// The database handle.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// The ref notice registry, shared by every clone of the state.
    pub fn notices(&self) -> &Notices {
        &self.notices
    }

    /// The validated server configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }
}

/// The OIDC state for the configured provider, if any. A client that cannot be built is logged
/// and leaves OIDC off, so the rest of the server still serves.
fn oidc_state(config: &Config) -> Option<OidcState> {
    let settings = config.oidc()?;
    let https_only = url::Url::parse(settings.issuer()).is_ok_and(|url| url.scheme() == "https");
    match IdpHttp::new(https_only) {
        Ok(http) => Some(OidcState::new(
            Provider::new(settings.clone(), http),
            Timings::default(),
        )),
        Err(err) => {
            tracing::error!("cannot build the OIDC client ({err}); serving without OIDC sign-in");
            None
        }
    }
}

fn git_engine(config: &Config, limits: GitLimits) -> GitEngine {
    let repos = Repos::new(config.repos_dir(), config.git().to_path_buf());
    GitEngine::new(
        repos,
        config.hooks_dir(),
        config.tmp_dir(),
        config.max_push_bytes(),
        limits,
    )
}

/// Every route under `/api/v1` and the git smart HTTP routes under `/git`; unknown paths and
/// wrong methods answer `not_found`.
pub(crate) fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/audit", get(audit::list))
        .route("/health", get(system::health))
        .route("/server", get(system::server))
        .route("/session", post(session::create))
        .route("/me", get(tokens::me))
        .route("/oidc/start", get(oidc::start))
        .route("/oidc/callback", get(oidc::callback))
        .route("/teams", get(teams::list).post(teams::create))
        .route(
            "/teams/{team}/members/{username}",
            put(teams::add_member).delete(teams::remove_member),
        )
        .route("/teams/{team}/workspaces", post(workspaces::create))
        .route("/update", get(update::get))
        .route("/update/check", post(update::check))
        .route("/tokens", get(tokens::list).post(tokens::create))
        .route("/tokens/{id}", delete(tokens::revoke))
        .route("/users", get(users::list).post(users::create))
        .route("/users/{username}", patch(users::patch))
        .route("/users/{username}/tokens", get(tokens::list_for_user))
        .route("/workspaces", get(workspaces::list))
        .route(
            "/workspaces/{team}/{ws}",
            get(workspaces::get).delete(workspaces::delete),
        )
        .route("/workspaces/{team}/{ws}/events", get(events::stream))
        .route("/workspaces/{team}/{ws}/grants", get(grants::list))
        .route(
            "/workspaces/{team}/{ws}/grants/{kind}/{name}",
            put(grants::put).delete(grants::delete),
        )
        .route(
            "/workspaces/{team}/{ws}/overrides",
            get(overrides::list).post(overrides::arm),
        )
        .route(
            "/workspaces/{team}/{ws}/overrides/{id}",
            delete(overrides::disarm),
        );
    // The method fallback is set after nesting so it reaches the nested routes too; a wrong
    // method is answered like an unknown path rather than with a bare 405.
    Router::new()
        .nest("/api/v1", v1)
        .merge(crate::git::http::router())
        .fallback(not_found)
        .method_not_allowed_fallback(not_found)
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(state.metrics()),
            crate::metrics::http::layer,
        ))
        .with_state(state)
}

async fn not_found() -> ErrorResponse {
    ApiError::not_found("no such route").into()
}
