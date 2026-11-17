//! Server-level endpoints: liveness and what this server offers.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use super::AppState;
use super::error::ApiResult;
use crate::kinds;

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
    kinds: BTreeMap<&'static str, KindInfo>,
}

#[derive(Serialize)]
struct AuthMethods {
    password: bool,
    oidc: bool,
}

#[derive(Serialize)]
struct KindInfo {
    formats: &'static [&'static str],
}

/// `GET /api/v1/health`: ok while the database answers a ping, else 503 `unavailable`.
pub(super) async fn health(State(state): State<AppState>) -> ApiResult<Json<Health>> {
    state.db().ping().await?;
    Ok(Json(Health {
        status: "ok",
        version: VERSION,
    }))
}

/// `GET /api/v1/server`: the version, the API generation, the sign-in methods offered and
/// each kind's accepted workspace formats, keyed by kind name in sorted order.
pub(super) async fn server() -> Json<ServerInfo> {
    Json(ServerInfo {
        version: VERSION,
        api: 1,
        auth: AuthMethods {
            password: true,
            oidc: false,
        },
        kinds: kinds::all()
            .iter()
            .map(|rules| {
                let info = KindInfo {
                    formats: rules.formats(),
                };
                (rules.kind(), info)
            })
            .collect(),
    })
}
