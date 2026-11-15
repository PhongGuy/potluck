//! The JSON API under `/api/v1`: shared state, the router and the error model.

use std::sync::Arc;

use axum::Router;
use axum::routing::get;

use crate::config::Config;
use crate::db::Db;
use crate::git::access::{DenyAll, GitAccess};
use crate::git::{GitEngine, GitLimits, Repos};

mod actor;
pub mod credential;
pub mod error;
mod system;

pub use actor::Actor;
use error::ApiError;

/// What every handler can reach: the database handle, the validated config, the git access
/// resolver and the git engine.
#[derive(Clone)]
#[non_exhaustive]
pub struct AppState {
    db: Db,
    config: Arc<Config>,
    git_access: Arc<dyn GitAccess>,
    git: Arc<GitEngine>,
}

impl AppState {
    /// Bundles the handle and the config for the router, with the deny-all resolver and the
    /// default git limits; the engine reads its paths and body cap from `config`.
    pub fn new(db: Db, config: Config) -> Self {
        let git = Arc::new(git_engine(&config, GitLimits::default()));
        AppState {
            db,
            config: Arc::new(config),
            git_access: Arc::new(DenyAll),
            git,
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
        .route("/server", get(system::server));
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
