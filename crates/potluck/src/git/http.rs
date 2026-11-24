//! The smart HTTP routes: authorize the caller for the service, then hand the request to git.

use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, EXPIRES, PRAGMA,
};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::api::AppState;
use crate::api::credential::credential_from_headers;
use crate::api::error::{ApiError, ApiResult};
use crate::git::access::{Access, PushContext};
use crate::git::engine::PushObserver;
use crate::git::push::RefMove;
use crate::git::{GitError, Service};
use crate::role::Role;
use crate::slug::Slug;

/// `{team}` and `{repo}` from the URL, or the rejection, so a bad segment answers our 404.
type RepoPath = Result<Path<(String, String)>, PathRejection>;

/// The smart HTTP routes, mounted beside `/api/v1` by `api::router`:
/// `GET /git/{team}/{repo}/info/refs?service=`, `POST /git/{team}/{repo}/git-upload-pack`,
/// `POST /git/{team}/{repo}/git-receive-pack`. `{repo}` is `{ws}.git`.
pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/git/{team}/{repo}/info/refs", get(info_refs))
        .route("/git/{team}/{repo}/git-upload-pack", post(upload_pack))
        .route("/git/{team}/{repo}/git-receive-pack", post(receive_pack))
}

async fn info_refs(
    State(state): State<AppState>,
    path: RepoPath,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let name = query
        .as_deref()
        .and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("service=")));
    let Some(name) = name else {
        return Err(ApiError::invalid_request(
            "smart HTTP only: the service query parameter is required",
        )
        .into());
    };
    let service = Service::from_name(name)
        .ok_or_else(|| ApiError::invalid_value("service", "unknown service"))?;
    let (repo, _access) = authorize(&state, &headers, path, service).await?;
    let body = state
        .git()
        .advertise(service, &repo, wants_v2(&headers))
        .await?;
    Ok(git_response(service.advertisement_type(), Body::from(body)))
}

async fn upload_pack(
    State(state): State<AppState>,
    path: RepoPath,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    rpc(&state, path, &headers, body, Service::UploadPack).await
}

async fn receive_pack(
    State(state): State<AppState>,
    path: RepoPath,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    rpc(&state, path, &headers, body, Service::ReceivePack).await
}

/// One POST: a declared length over the cap is refused before a byte is read; otherwise git
/// gets the body and the resolver's hook environment, and its output streams back.
async fn rpc(
    state: &AppState,
    path: RepoPath,
    headers: &HeaderMap,
    body: Body,
    service: Service,
) -> ApiResult<Response> {
    let (repo, access) = authorize(state, headers, path, service).await?;
    let limit = state.git().max_body();
    let declared = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|len| len > limit) {
        return Err(GitError::TooLarge { limit }.into());
    }
    // A header that is not text cannot be gzip, so it is refused rather than read as absent.
    let encoding = match headers.get(CONTENT_ENCODING) {
        None => None,
        Some(v) => Some(
            v.to_str()
                .map_err(|_| ApiError::invalid_request("unsupported Content-Encoding"))?,
        ),
    };
    // Only a receive-pack the resolver tied to a workspace is watched; everything else is served
    // without bookkeeping.
    let on_push = match (service, access.push()) {
        (Service::ReceivePack, Some(ctx)) => {
            Some(observer(state, ctx.clone(), repo.clone(), access.role()))
        }
        _ => None,
    };
    let out = state
        .git()
        .rpc(
            service,
            &repo,
            wants_v2(headers),
            encoding,
            access.hook_env(),
            body,
            on_push,
        )
        .await?;
    Ok(git_response(service.result_type(), out))
}

/// Hands a push's confirmed moves to the bookkeeping, then to the admin re-check of `repo`. It
/// runs on the reaper task, so both are spawned rather than awaited.
fn observer(state: &AppState, ctx: PushContext, repo: PathBuf, role: Role) -> PushObserver {
    let state = state.clone();
    let tasks = state.git().tasks().clone();
    Box::new(move |moves: Vec<RefMove>, _report| {
        // The re-check needs the moves after the record has consumed them, and only an admin
        // or override push is re-checked, so only that push copies them.
        let updates = if role == Role::Admin || ctx.armed_override().is_some() {
            crate::api::pushes::ref_updates(&moves)
        } else {
            Vec::new()
        };
        tasks.spawn(async move {
            crate::api::pushes::record(state.clone(), ctx.clone(), moves).await;
            crate::api::pushes::recheck_admin_push(&state, &ctx, repo, role, updates).await;
        });
    })
}

/// The checks every route shares, in the order that leaks least: the repository name, the
/// credential, the resolver's grant, the role the service needs, and finally the disk.
async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    path: RepoPath,
    service: Service,
) -> Result<(PathBuf, Access), ApiError> {
    let no_repo = || ApiError::not_found("no such repository");
    let Ok(Path((team, repo))) = path else {
        return Err(no_repo());
    };
    let ws = repo.strip_suffix(".git").ok_or_else(no_repo)?;
    let team = Slug::parse(&team).map_err(|_| no_repo())?;
    let ws = Slug::parse(ws).map_err(|_| no_repo())?;
    let credential = credential_from_headers(headers)?
        .ok_or_else(|| ApiError::unauthenticated("a token is required"))?;
    let access = state.git_access().resolve(&credential, &team, &ws).await?;
    if access.role() < service.required_role() {
        let verb = match service {
            Service::UploadPack => "fetch",
            Service::ReceivePack => "push",
        };
        return Err(ApiError::forbidden(format!(
            "role {} cannot {verb}",
            access.role().as_str()
        )));
    }
    let repo = state.git().repos().path(&team, &ws);
    let is_dir = tokio::fs::metadata(&repo).await.is_ok_and(|m| m.is_dir());
    if !is_dir {
        tracing::error!(path = %repo.display(), "repository missing");
        return Err(ApiError::internal("repository missing"));
    }
    Ok((repo, access))
}

/// Protocol v2 is asked for with exactly `Git-Protocol: version=2`.
fn wants_v2(headers: &HeaderMap) -> bool {
    headers.get("git-protocol").and_then(|v| v.to_str().ok()) == Some("version=2")
}

/// A 200 with git's content type and the headers that keep proxies from caching refs.
fn git_response(content_type: &'static str, body: Body) -> Response {
    (
        [
            (CONTENT_TYPE, content_type),
            (CACHE_CONTROL, "no-cache, max-age=0, must-revalidate"),
            (PRAGMA, "no-cache"),
            (EXPIRES, "Fri, 01 Jan 1980 00:00:00 GMT"),
        ],
        body,
    )
        .into_response()
}
