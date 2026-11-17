//! The update notice for server admins: what the last release check found, and a check on demand.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use super::AppState;
use super::auth::Authenticated;
use super::error::{ApiError, ApiResult};
use crate::update::{Release, UpdateStatus};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize)]
pub(super) struct UpdateJson {
    current: String,
    latest: Option<ReleaseJson>,
    checked_at: Option<String>,
    check: &'static str,
}

#[derive(Serialize)]
struct ReleaseJson {
    version: String,
    digest: String,
    published: String,
    notes_url: String,
}

impl From<&Release> for ReleaseJson {
    fn from(release: &Release) -> Self {
        ReleaseJson {
            version: release.version().to_owned(),
            digest: release.digest().to_owned(),
            published: release.published().to_owned(),
            notes_url: release.notes_url().to_owned(),
        }
    }
}

// The schedule is "daily" only when a source is wired and the flag left it on.
fn schedule(state: &AppState) -> &'static str {
    if state.updates().is_some() && state.config().update_check() {
        "daily"
    } else {
        "off"
    }
}

fn update_json(state: &AppState, status: &UpdateStatus) -> UpdateJson {
    UpdateJson {
        current: status.current().to_owned(),
        latest: status.latest().map(ReleaseJson::from),
        checked_at: status.checked_at().map(str::to_owned),
        check: schedule(state),
    }
}

/// `GET /api/v1/update` (server admin or `System`): the status as of the last check; with no
/// source, the running version and nothing checked.
pub(super) async fn get(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<UpdateJson>> {
    auth.require_server_admin()?;
    Ok(Json(match state.updates() {
        Some(checker) => update_json(&state, &checker.status()),
        None => UpdateJson {
            current: VERSION.to_owned(),
            latest: None,
            checked_at: None,
            check: schedule(&state),
        },
    }))
}

/// `POST /api/v1/update/check` (server admin or `System`): checks now, even with the daily check
/// off; 503 `unavailable` with no source, or naming the failure class when the check fails.
pub(super) async fn check(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<UpdateJson>> {
    auth.require_server_admin()?;
    // Cloned out of the state so the awaited future holds no borrow of it.
    let checker = state
        .updates()
        .cloned()
        .ok_or_else(|| ApiError::unavailable("no update source is configured"))?;
    let status = checker
        .check()
        .await
        .map_err(|e| ApiError::unavailable(e.to_string()))?;
    Ok(Json(update_json(&state, &status)))
}
