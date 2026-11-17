//! Overrides on one workspace: armed by its admins, consumed by the next accepted push.

use axum::Json;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;

use super::AppState;
use super::actor::Actor;
use super::auth::Authenticated;
use super::error::{ApiError, ApiResult, ErrorResponse};
use super::json::ApiJson;
use super::objects::OverrideJson;
use super::workspaces::{WsPath, load, require};
use crate::db::overrides::Disarmed;
use crate::role::Role;

/// The longest `reason`, in characters, after trimming.
const MAX_REASON: usize = 1000;

/// `{team}`, `{ws}`, `{id}`, or the rejection (404).
type OverridePath = Result<Path<(String, String, String)>, PathRejection>;

/// The `POST W/overrides` body.
#[derive(Debug, Deserialize)]
pub(super) struct ArmOverride {
    for_user: String,
    reason: String,
}

/// `GET W/overrides` (admin): `[Override]`, oldest first.
pub(super) async fn list(
    State(state): State<AppState>,
    auth: Authenticated,
    path: WsPath,
) -> ApiResult<Json<Vec<OverrideJson>>> {
    let (ws, role) = load(&state, &auth, path).await?;
    require(role, Role::Admin, "manage overrides")?;
    let overrides = state.db().list_overrides(ws.id()).await?;
    Ok(Json(overrides.iter().map(OverrideJson::from).collect()))
}

/// `POST W/overrides {for_user, reason}` (admin, checked before the body is read): 422
/// `for_user` for an unknown or disabled user; 422 `reason` unless 1 to 1000 characters after
/// trimming; 409 while one is already armed for that user; 201 Override with `armed_by` the
/// caller's username, or `system`.
pub(super) async fn arm(
    State(state): State<AppState>,
    auth: Authenticated,
    path: WsPath,
    body: Result<ApiJson<ArmOverride>, ErrorResponse>,
) -> ApiResult<(StatusCode, Json<OverrideJson>)> {
    let (ws, role) = load(&state, &auth, path).await?;
    require(role, Role::Admin, "manage overrides")?;
    let ApiJson(body) = body?;
    let user = match state.db().user_by_username(&body.for_user).await? {
        Some(user) if !user.disabled() => user,
        _ => {
            return Err(ApiError::invalid_value("for_user", "no such active user").into());
        }
    };
    let reason = body.reason.trim();
    if reason.is_empty() || reason.chars().count() > MAX_REASON {
        return Err(
            ApiError::invalid_value("reason", "reason must be 1 to 1000 characters").into(),
        );
    }
    // Two admins racing past this check is harmless: the hook is handed the lowest armed id
    // and the push consumes exactly that one.
    if state
        .db()
        .armed_override(ws.id(), user.id())
        .await?
        .is_some()
    {
        return Err(ApiError::conflict("an override is already armed for this user").into());
    }
    let armed_by = actor_name(&auth);
    let id = state
        .db()
        .arm_override(ws.id(), user.id(), reason, armed_by)
        .await?;
    let Some(record) = state.db().override_by_id(ws.id(), id).await? else {
        tracing::error!(id, "an override vanished right after it was armed");
        return Err(ApiError::internal("override could not be read back").into());
    };
    tracing::info!(
        workspace = %format!("{}/{}", ws.team_slug(), ws.slug()),
        for_user = user.username(),
        armed_by,
        id,
        "override armed"
    );
    Ok((StatusCode::CREATED, Json(OverrideJson::from(&record))))
}

/// `DELETE W/overrides/{id}` (admin): 204 for an armed override, 409 for a consumed one, 404 for
/// a missing or non-numeric id.
pub(super) async fn disarm(
    State(state): State<AppState>,
    auth: Authenticated,
    path: OverridePath,
) -> ApiResult<StatusCode> {
    // The role is checked before the id is parsed, so a caller without admin learns nothing.
    let (ws_path, id) = match path {
        Ok(Path((team, ws, id))) => (Ok(Path((team, ws))), id.parse::<i64>().ok()),
        Err(rejection) => (Err(rejection), None),
    };
    let (ws, role) = load(&state, &auth, ws_path).await?;
    require(role, Role::Admin, "manage overrides")?;
    let Some(id) = id else {
        return Err(ApiError::not_found("no such override").into());
    };
    match state.db().disarm_override(ws.id(), id).await? {
        Disarmed::Removed => {
            let disarmed_by = actor_name(&auth);
            tracing::info!(
                workspace = %format!("{}/{}", ws.team_slug(), ws.slug()),
                disarmed_by,
                id,
                "override disarmed"
            );
            Ok(StatusCode::NO_CONTENT)
        }
        Disarmed::Consumed => {
            Err(ApiError::conflict("a consumed override stays in the history").into())
        }
        Disarmed::Missing => Err(ApiError::not_found("no such override").into()),
    }
}

/// The caller's username, or `system`; `load` has already refused `Anonymous`.
fn actor_name(auth: &Authenticated) -> &str {
    match auth.actor() {
        Actor::User(caller) => caller.user().username(),
        // Only `System` reaches this arm, since `load` has already refused `Anonymous`.
        _ => "system",
    }
}
