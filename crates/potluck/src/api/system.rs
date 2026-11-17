//! Server-level endpoints: liveness and what this server offers.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use super::AppState;
use super::error::ApiResult;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize)]
pub(super) struct Health {
    status: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
pub(super) struct ServerInfo {
    version: &'static str,
    api: u32,
    auth: AuthMethods,
}

#[derive(Serialize)]
struct AuthMethods {
    password: bool,
    oidc: bool,
}

/// `GET /api/v1/health`: ok while the database answers a ping, else 503 `unavailable`.
pub(super) async fn health(State(state): State<AppState>) -> ApiResult<Json<Health>> {
    state.db().ping().await?;
    Ok(Json(Health {
        status: "ok",
        version: VERSION,
    }))
}

/// `GET /api/v1/server`: the version, the API generation and the sign-in methods offered.
pub(super) async fn server() -> Json<ServerInfo> {
    Json(ServerInfo {
        version: VERSION,
        api: 1,
        auth: AuthMethods {
            password: true,
            oidc: false,
        },
    })
}
