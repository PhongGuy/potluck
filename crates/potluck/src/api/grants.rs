//! Grants on one workspace, managed by its admins.

use axum::Json;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;

use super::AppState;
use super::auth::Authenticated;
use super::error::ApiError;
use super::events;
use super::json::ApiJson;
use super::objects::GrantJson;
use super::workspaces::{WsPath, load, require};
use crate::db::grants::Subject;
use crate::db::workspaces::WorkspaceRecord;
use crate::role::Role;

/// `{team}`, `{ws}`, `{kind}`, `{name}`, or the rejection (404).
type GrantPath = Result<Path<(String, String, String, String)>, PathRejection>;

/// The `PUT W/grants/{kind}/{name}` body.
#[derive(Debug, Deserialize)]
pub(super) struct PutGrant {
    role: String,
}

/// `GET W/grants` (admin): `[Grant]`, teams first then users, by name.
pub(super) async fn list(
    State(state): State<AppState>,
    auth: Authenticated,
    path: WsPath,
) -> Result<Json<Vec<GrantJson>>, ApiError> {
    let (ws, role) = load(&state, &auth, path).await?;
    require(role, Role::Admin, "manage grants")?;
    let grants = state.db().list_grants(ws.id()).await?;
    Ok(Json(grants.iter().map(GrantJson::from).collect()))
}

/// `PUT W/grants/{kind}/{name} {role}` (admin, checked before the body is read): `kind` is
/// `user` or `team`, else 404; 404 for an unknown user or team; 422 `role` unless `read`,
/// `write` or `admin`; 200 Grant.
pub(super) async fn put(
    State(state): State<AppState>,
    auth: Authenticated,
    path: GrantPath,
    body: Result<ApiJson<PutGrant>, ApiError>,
) -> Result<Json<GrantJson>, ApiError> {
    let (ws, subject) = load_subject(&state, &auth, path).await?;
    let ApiJson(body) = body?;
    let Some(role) = Role::parse(&body.role) else {
        return Err(ApiError::invalid_value(
            "role",
            "role must be read, write or admin",
        ));
    };
    match state.db().put_grant(ws.id(), subject, role).await? {
        Some(grant) => Ok(Json(GrantJson::from(&grant))),
        None => Err(ApiError::not_found("no such user or team")),
    }
}

/// `DELETE W/grants/{kind}/{name}` (admin): 204; 404 when there is no such grant.
pub(super) async fn delete(
    State(state): State<AppState>,
    auth: Authenticated,
    path: GrantPath,
) -> Result<StatusCode, ApiError> {
    let (ws, subject) = load_subject(&state, &auth, path).await?;
    if !state.db().delete_grant(ws.id(), subject.clone()).await? {
        return Err(ApiError::not_found("no such grant"));
    }
    // Everyone who held the grant may have lost their last way in; their streams close here.
    let names = match subject {
        Subject::User(name) => vec![name],
        Subject::Team(slug) => match state.db().team_by_slug(&slug).await? {
            Some(team) => state.db().team_members(team.id()).await?,
            None => Vec::new(),
        },
    };
    let mut users = Vec::with_capacity(names.len());
    for name in names {
        users.extend(state.db().user_by_username(&name).await?);
    }
    events::reap_unreadable(&state, &users).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The workspace (the caller must be its admin) and the grant's subject from the path. The
/// role is checked before the kind, so a caller without admin learns nothing from the URL.
async fn load_subject(
    state: &AppState,
    auth: &Authenticated,
    path: GrantPath,
) -> Result<(WorkspaceRecord, Subject), ApiError> {
    let (ws_path, rest) = match path {
        Ok(Path((team, ws, kind, name))) => (Ok(Path((team, ws))), Some((kind, name))),
        Err(rejection) => (Err(rejection), None),
    };
    let (ws, role) = load(state, auth, ws_path).await?;
    require(role, Role::Admin, "manage grants")?;
    let subject = match rest {
        Some((kind, name)) if kind == "user" => Subject::User(name),
        Some((kind, name)) if kind == "team" => Subject::Team(name),
        _ => return Err(ApiError::not_found("no such route")),
    };
    Ok((ws, subject))
}
