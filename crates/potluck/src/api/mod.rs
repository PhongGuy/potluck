//! The JSON API under `/api/v1`: shared state, the router and the error model.

use std::sync::Arc;

use axum::Router;
use axum::routing::get;

use crate::config::Config;
use crate::db::Db;

pub mod error;
mod system;

use error::ApiError;

/// What every handler can reach: the database handle and the validated config (later
/// sub-plans read `data`, `git` and `max_push_bytes` from it).
#[derive(Clone)]
#[non_exhaustive]
pub struct AppState {
    db: Db,
    config: Arc<Config>,
}

impl AppState {
    /// Bundles the handle and the config for the router.
    pub fn new(db: Db, config: Config) -> Self {
        AppState {
            db,
            config: Arc::new(config),
        }
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

/// Every route under `/api/v1`; unknown paths and wrong methods answer `not_found`.
pub fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/health", get(system::health))
        .route("/server", get(system::server));
    // The method fallback is set after nesting so it reaches the nested routes too; a wrong
    // method is answered like an unknown path rather than with a bare 405.
    Router::new()
        .nest("/api/v1", v1)
        .fallback(not_found)
        .method_not_allowed_fallback(not_found)
        .with_state(state)
}

async fn not_found() -> ApiError {
    ApiError::not_found("no such route")
}
