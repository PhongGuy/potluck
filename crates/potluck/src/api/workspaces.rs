//! Workspaces: creation with the seed or the import state, the readable list, one workspace.

use axum::Json;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde::Deserialize;

use super::auth::{Authenticated, token_required};
use super::error::ApiError;
use super::json::ApiJson;
use super::objects::{WorkspaceJson, noreply_host};
use super::teams::{check_name, check_slug};
use super::{Actor, AppState};
use crate::db::workspaces::{NewWorkspace, WorkspaceRecord};
use crate::db::{Db, DbError};
use crate::git::RepoError;
use crate::kinds;
use crate::role::Role;
use crate::slug::Slug;

/// The `POST /teams/{team}/workspaces` body; `seed` absent means `true`.
#[derive(Debug, Deserialize)]
pub(super) struct CreateWorkspace {
    slug: String,
    name: String,
    kind: String,
    default_branch: String,
    seed: Option<bool>,
}

/// `POST /teams/{team}/workspaces {slug, name, kind, default_branch, seed?}` (server admin or
/// `System`, checked before the body is read): 404 unknown team; 422 `slug`, `name` (blank or
/// over 200 chars trimmed), `kind` (`kinds::by_name` is `None`), `default_branch`; 409 when
/// `{team}/{slug}` exists or its directory does. Otherwise, in one spawned task the handler
/// awaits so a dropped request cannot stop it half way: the repository is initialised, seeded
/// with the kind's files as `Potluck <potluck@users.noreply.{host}>` (`seed` absent or true) or
/// left empty with the import state set, and the row is inserted; a failure after the claim
/// removes the directory. 201 Workspace with role `admin`.
pub(super) async fn create(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(team): Path<String>,
    body: Result<ApiJson<CreateWorkspace>, ApiError>,
) -> Result<(StatusCode, Json<WorkspaceJson>), ApiError> {
    auth.require_server_admin()?;
    let ApiJson(body) = body?;
    let slug = check_slug(&body.slug)?;
    let name = check_name(&body.name)?.to_owned();
    // The kind is checked before the claim: the schema CHECK would admit a reserved kind, and
    // an unknown one would only surface from the insert as a 500.
    let Some(kind) = kinds::by_name(&body.kind) else {
        return Err(ApiError::invalid_value(
            "kind",
            "kind must be a supported workspace kind",
        ));
    };
    check_default_branch(&body.default_branch)?;
    let team = match Slug::parse(&team) {
        Ok(slug) => state
            .db()
            .team_by_slug(slug.as_str())
            .await?
            .map(|t| (slug, t)),
        Err(_) => None,
    };
    let Some((team_slug, team)) = team else {
        return Err(ApiError::not_found("no such team"));
    };

    let repos = state.git().repos().clone();
    let db = state.db().clone();
    let branch = body.default_branch;
    let seed = body.seed.unwrap_or(true);
    let email = format!(
        "potluck@users.noreply.{}",
        noreply_host(state.config().public_url())
    );
    let row =
        NewWorkspace::new(team.id(), &slug, &name, kind.name(), &branch).import_pending(!seed);
    // A spawned task runs to its end even when axum drops this future on a disconnect, so a
    // claimed directory is always either completed with its row or removed again.
    let task = tokio::spawn(async move {
        let repo = repos
            .init(&team_slug, &slug, &branch)
            .await
            .map_err(repo_error)?;
        let made = async {
            if seed {
                let files = kind.seed();
                let pairs: Vec<(&str, &[u8])> =
                    files.iter().map(|f| (f.path(), f.bytes())).collect();
                let message = format!("Seed {} workspace", kind.name());
                repos
                    .seed_commit(&repo, &branch, &pairs, "Potluck", &email, &message)
                    .await
                    .map_err(repo_error)?;
            }
            Ok::<_, ApiError>(db.create_workspace(row).await?)
        }
        .await;
        if made.is_err()
            && let Err(err) = repos.remove(&team_slug, &slug).await
        {
            tracing::error!(error = %err, "removing a half-made workspace repository failed");
        }
        made
    });
    let record = task.await.map_err(|err| {
        tracing::error!(error = %err, "workspace creation task failed");
        ApiError::internal("workspace creation failed")
    })??;
    Ok((
        StatusCode::CREATED,
        Json(WorkspaceJson::new(
            &record,
            Role::Admin,
            state.config().public_url(),
        )),
    ))
}

/// `{team}` and `{ws}` from the URL, or the rejection, so a bad segment answers our 404.
pub(super) type WsPath = Result<Path<(String, String)>, PathRejection>;

/// `GET /workspaces?kind=` (self; `System` and server admins see everything at `admin`): the
/// caller's held workspaces, by team then slug, only those of `kind` when the query has one.
pub(super) async fn list(
    State(state): State<AppState>,
    auth: Authenticated,
    RawQuery(query): RawQuery,
) -> Result<Json<Vec<WorkspaceJson>>, ApiError> {
    // An unknown kind is a filter that matches nothing, not a validation error.
    let kind = query
        .as_deref()
        .and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("kind=")));
    let mut list = held_records(&state, auth.actor()).await?;
    if let Some(kind) = kind {
        list.retain(|(ws, _)| ws.kind() == kind);
    }
    Ok(Json(to_json(&state, &list)))
}

/// `GET /workspaces/{team}/{ws}` (read): the Workspace with the caller's role.
pub(super) async fn get(
    State(state): State<AppState>,
    auth: Authenticated,
    path: WsPath,
) -> Result<Json<WorkspaceJson>, ApiError> {
    let (ws, role) = load(&state, &auth, path).await?;
    require(role, Role::Read, "read")?;
    Ok(Json(WorkspaceJson::new(
        &ws,
        role,
        state.config().public_url(),
    )))
}

/// `DELETE /workspaces/{team}/{ws}` (admin): in one spawned task the handler awaits, the row
/// goes first (grants and overrides cascade), then the repository directory; 204. A directory
/// already gone is fine; a removal failure is logged and 500, with the workspace already gone
/// from the API.
pub(super) async fn delete(
    State(state): State<AppState>,
    auth: Authenticated,
    path: WsPath,
) -> Result<StatusCode, ApiError> {
    let (ws, role) = load(&state, &auth, path).await?;
    require(role, Role::Admin, "delete")?;
    let (Ok(team_slug), Ok(slug)) = (Slug::parse(ws.team_slug()), Slug::parse(ws.slug())) else {
        tracing::error!(id = ws.id(), "a stored workspace slug does not parse");
        return Err(ApiError::internal("workspace could not be deleted"));
    };
    let repos = state.git().repos().clone();
    let db = state.db().clone();
    // Row first, so a git request never meets a live workspace without its repository; the
    // spawned task finishes both steps even when axum drops this future on a disconnect.
    let task = tokio::spawn(async move {
        if !db.delete_workspace(ws.id()).await? {
            return Err(ApiError::not_found("no such workspace"));
        }
        if let Err(err) = repos.remove(&team_slug, &slug).await {
            tracing::error!(id = ws.id(), error = %err, "removing a deleted workspace's repository failed");
            return Err(ApiError::internal("repository could not be removed"));
        }
        Ok(StatusCode::NO_CONTENT)
    });
    task.await.map_err(|err| {
        tracing::error!(error = %err, "workspace deletion task failed");
        ApiError::internal("workspace deletion failed")
    })?
}

/// The one place a role is decided: `System` and a `server_admin` user are `Admin` on every
/// workspace; any other user is the highest grant (`Db::role_for`), `None` without one;
/// `Anonymous` is `None`. A disabled user never gets here (`authenticate` refuses the token).
pub(crate) async fn workspace_role(
    db: &Db,
    actor: &Actor,
    ws: &WorkspaceRecord,
) -> Result<Option<Role>, DbError> {
    if actor.is_server_admin() {
        return Ok(Some(Role::Admin));
    }
    match actor.user() {
        Some(user) => db.role_for(ws.id(), user.user().id()).await,
        None => Ok(None),
    }
}

/// The workspace and the caller's role through `workspace_role`: 401 `Anonymous`; an unknown
/// workspace, a non-slug segment or a user without a grant is 404 "no such workspace".
pub(super) async fn load(
    state: &AppState,
    auth: &Authenticated,
    path: WsPath,
) -> Result<(WorkspaceRecord, Role), ApiError> {
    if let Actor::Anonymous = auth.actor() {
        return Err(token_required());
    }
    let no_ws = || ApiError::not_found("no such workspace");
    let Ok(Path((team, ws))) = path else {
        return Err(no_ws());
    };
    let (Ok(team), Ok(ws)) = (Slug::parse(&team), Slug::parse(&ws)) else {
        return Err(no_ws());
    };
    let Some(record) = state
        .db()
        .workspace_by_slugs(team.as_str(), ws.as_str())
        .await?
    else {
        return Err(no_ws());
    };
    // Without a role the workspace is hidden, answered exactly like one that does not exist.
    match workspace_role(state.db(), auth.actor(), &record).await? {
        Some(role) => Ok((record, role)),
        None => Err(no_ws()),
    }
}

/// 403 "role {have} cannot {verb}" when `have < need`.
pub(super) fn require(have: Role, need: Role, verb: &str) -> Result<(), ApiError> {
    if have < need {
        return Err(ApiError::forbidden(format!(
            "role {} cannot {verb}",
            have.as_str()
        )));
    }
    Ok(())
}

/// The caller's workspaces as `list` answers them, unfiltered; `/me` uses it.
pub(super) async fn held(state: &AppState, actor: &Actor) -> Result<Vec<WorkspaceJson>, ApiError> {
    Ok(to_json(state, &held_records(state, actor).await?))
}

/// Server admins and `System` hold every workspace at `admin`; a user holds their grants;
/// `Anonymous` is 401.
async fn held_records(
    state: &AppState,
    actor: &Actor,
) -> Result<Vec<(WorkspaceRecord, Role)>, ApiError> {
    if actor.is_server_admin() {
        let all = state.db().list_workspaces().await?;
        return Ok(all.into_iter().map(|ws| (ws, Role::Admin)).collect());
    }
    let Some(user) = actor.user() else {
        return Err(token_required());
    };
    let held = state.db().workspaces_for_user(user.user().id()).await?;
    Ok(held
        .into_iter()
        .map(|h| (h.workspace().clone(), h.role()))
        .collect())
}

fn to_json(state: &AppState, list: &[(WorkspaceRecord, Role)]) -> Vec<WorkspaceJson> {
    let url = state.config().public_url();
    list.iter()
        .map(|(ws, role)| WorkspaceJson::new(ws, *role, url))
        .collect()
}

/// A directory already there is the client's conflict; any other repository failure is logged
/// and answered with a fixed message.
fn repo_error(err: RepoError) -> ApiError {
    match err {
        RepoError::Exists { path } => ApiError::conflict(format!(
            "repository directory {} already exists",
            path.display()
        )),
        err => {
            tracing::error!(error = %err, "repository error");
            ApiError::internal("repository error")
        }
    }
}

/// The branch names `git check-ref-format --branch` accepts within
/// `^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$`: no `..`, no trailing `.` or `.lock`, and not `HEAD`,
/// which `git init` would accept as `refs/heads/HEAD`. Else 422 `default_branch`.
fn check_default_branch(s: &str) -> Result<(), ApiError> {
    let mut chars = s.chars();
    let shaped = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.len() <= 100
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if shaped && !s.contains("..") && !s.ends_with('.') && !s.ends_with(".lock") && s != "HEAD" {
        Ok(())
    } else {
        Err(ApiError::invalid_value(
            "default_branch",
            "default_branch must match ^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$, without `..`, a \
             trailing `.` or `.lock`, and not be HEAD",
        ))
    }
}
