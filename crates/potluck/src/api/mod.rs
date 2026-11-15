//! The JSON API under `/api/v1`: shared state, the router and the error model.

use std::sync::Arc;

use axum::Router;
use axum::routing::{delete, get, patch, post, put};

use crate::auth::password::Passwords;
use crate::auth::rate_limit::SessionLimits;
use crate::config::Config;
use crate::db::Db;
use crate::git::access::{DenyAll, GitAccess};
use crate::git::{GitEngine, GitLimits, Repos};

mod actor;
pub mod auth;
pub mod client_ip;
pub mod credential;
pub mod error;
pub mod json;
pub mod objects;
mod session;
mod system;
mod teams;
mod tokens;
mod users;

pub use actor::{Actor, AuthUser};
use error::ApiError;

/// What every handler can reach: the database handle, the validated config, the git access
/// resolver, the git engine, the password hasher and the sign-in rate limiters.
#[derive(Clone)]
#[non_exhaustive]
pub struct AppState {
    db: Db,
    config: Arc<Config>,
    git_access: Arc<dyn GitAccess>,
    git: Arc<GitEngine>,
    passwords: Passwords,
    session_limits: Arc<SessionLimits>,
}

impl AppState {
    /// Bundles the handle and the config for the router, with the deny-all resolver and the
    /// default git limits and the production hashing cost; the engine reads its paths and body
    /// cap from `config`.
    pub fn new(db: Db, config: Config) -> Self {
        let git = Arc::new(git_engine(&config, GitLimits::default()));
        AppState {
            db,
            config: Arc::new(config),
            git_access: Arc::new(DenyAll),
            git,
            passwords: Passwords::new(),
            session_limits: Arc::new(SessionLimits::default()),
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

    /// The engine.
    pub(crate) fn git(&self) -> &GitEngine {
        &self.git
    }

    /// The database handle.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// The validated server configuration.
    pub fn config(&self) -> &Config {
        &self.config
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
pub fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/health", get(system::health))
        .route("/server", get(system::server))
        .route("/session", post(session::create))
        .route("/me", get(tokens::me))
        .route("/teams", get(teams::list).post(teams::create))
        .route(
            "/teams/{team}/members/{username}",
            put(teams::add_member).delete(teams::remove_member),
        )
        .route("/tokens", get(tokens::list).post(tokens::create))
        .route("/tokens/{id}", delete(tokens::revoke))
        .route("/users", get(users::list).post(users::create))
        .route("/users/{username}", patch(users::patch))
        .route("/users/{username}/tokens", get(tokens::list_for_user));
    // The method fallback is set after nesting so it reaches the nested routes too; a wrong
    // method is answered like an unknown path rather than with a bare 405.
    Router::new()
        .nest("/api/v1", v1)
        .merge(crate::git::http::router())
        .fallback(not_found)
        .method_not_allowed_fallback(not_found)
        .with_state(state)
}

async fn not_found() -> ApiError {
    ApiError::not_found("no such route")
}
