//! Workspaces: creation with the seed or the import state, the readable list, one workspace.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;

use super::AppState;
use super::auth::Authenticated;
use super::error::ApiError;
use super::json::ApiJson;
use super::objects::{WorkspaceJson, noreply_host};
use super::teams::{check_name, check_slug};
use crate::db::workspaces::NewWorkspace;
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
