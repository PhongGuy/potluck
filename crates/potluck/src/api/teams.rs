//! Team management for server admins: `/teams` and `/teams/{team}/members/{username}`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;

use super::AppState;
use super::auth::Authenticated;
use super::error::{ApiError, ApiResult, ErrorResponse};
use super::events;
use super::json::ApiJson;
use super::objects::TeamJson;
use super::validate;
use crate::db::accounts::UserRecord;
use crate::db::teams::TeamRecord;
use crate::slug::Slug;

/// The `POST /teams` body.
#[derive(Debug, Deserialize)]
pub(super) struct CreateTeam {
    slug: String,
    name: String,
}

/// `GET /teams` (server admin or `System`): `[Team]` by slug.
pub(super) async fn list(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Vec<TeamJson>>> {
    auth.require_server_admin()?;
    let mut teams = Vec::new();
    for team in state.db().list_teams().await? {
        teams.push(team_json(&state, &team).await?);
    }
    Ok(Json(teams))
}

/// `POST /teams {slug, name}` (server admin or `System`, checked before the body is read):
/// 422 `slug` (not a slug) or `name` (blank or over 200 chars after trimming); 409 taken; 201 Team.
pub(super) async fn create(
    State(state): State<AppState>,
    auth: Authenticated,
    body: Result<ApiJson<CreateTeam>, ErrorResponse>,
) -> ApiResult<(StatusCode, Json<TeamJson>)> {
    // The body is extracted as a `Result` so a non-admin's bad body still answers 403.
    auth.require_server_admin()?;
    let ApiJson(body) = body?;
    let slug = check_slug(&body.slug)?;
    let name = validate::name("name", &body.name)?;
    let team = state.db().create_team(&slug, name).await?;
    Ok((StatusCode::CREATED, Json(TeamJson::new(&team, Vec::new()))))
}

/// `PUT /teams/{team}/members/{username}` (server admin or `System`): 404 unknown team or
/// user; 200 Team, already a member or not.
pub(super) async fn add_member(
    State(state): State<AppState>,
    auth: Authenticated,
    Path((team, username)): Path<(String, String)>,
) -> ApiResult<Json<TeamJson>> {
    auth.require_server_admin()?;
    let (team, user) = team_and_user(&state, &team, &username).await?;
    state.db().add_member(team.id(), user.id()).await?;
    Ok(Json(team_json(&state, &team).await?))
}

/// `DELETE /teams/{team}/members/{username}` (server admin or `System`): 204; 404 unknown
/// team or user, or not a member.
pub(super) async fn remove_member(
    State(state): State<AppState>,
    auth: Authenticated,
    Path((team, username)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    auth.require_server_admin()?;
    let (team, user) = team_and_user(&state, &team, &username).await?;
    if !state.db().remove_member(team.id(), user.id()).await? {
        return Err(ApiError::not_found("no such member").into());
    }
    // The membership is gone either way; a failed reap is logged rather than answered as 500.
    if let Err(err) = events::reap_unreadable(&state, std::slice::from_ref(&user)).await {
        tracing::error!(user = user.id(), error = ?err, "closing streams after a member removal failed");
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `s` as a slug, else 422 `slug`.
pub(super) fn check_slug(s: &str) -> Result<Slug, ApiError> {
    Slug::parse(s).map_err(|_| {
        ApiError::invalid_value(
            "slug",
            "slug must be 1 to 63 lowercase letters, digits or hyphens, starting with a letter \
             or digit",
        )
    })
}

async fn team_and_user(
    state: &AppState,
    team: &str,
    username: &str,
) -> Result<(TeamRecord, UserRecord), ApiError> {
    let Some(team) = state.db().team_by_slug(team).await? else {
        return Err(ApiError::not_found("no such team"));
    };
    let Some(user) = state.db().user_by_username(username).await? else {
        return Err(ApiError::not_found("no such user"));
    };
    Ok((team, user))
}

async fn team_json(state: &AppState, team: &TeamRecord) -> Result<TeamJson, ApiError> {
    Ok(TeamJson::new(
        team,
        state.db().team_members(team.id()).await?,
    ))
}
