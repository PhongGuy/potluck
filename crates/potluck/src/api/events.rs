//! `GET W/events`: the SSE stream of ref moves on one workspace.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use futures_util::stream;

use super::AppState;
use super::auth::Authenticated;
use super::error::ApiError;
use super::workspaces::{WsPath, load};
use crate::db::accounts::UserRecord;
use crate::git::push::list_refs;
use crate::slug::Slug;

/// `GET /workspaces/{team}/{ws}/events` (read; a user, so `System` is 403): `retry: 2000`, then
/// one `ref` event per branch as the repository stands, then one per moved ref as pushes land,
/// with the keep-alive comment every 15 s. Ends when the server closes it or the client lags.
/// Access is re-checked once subscribed, so a grant lost meanwhile gets 401 or 404, not a stream.
pub(super) async fn stream(
    State(state): State<AppState>,
    auth: Authenticated,
    path: WsPath,
) -> Result<Response, ApiError> {
    let (ws, _role) = load(&state, &auth, path).await?;
    let user = auth.require_user()?;
    let (Ok(team_slug), Ok(slug)) = (Slug::parse(ws.team_slug()), Slug::parse(ws.slug())) else {
        tracing::error!(id = ws.id(), "a stored workspace slug does not parse");
        return Err(ApiError::internal("events could not be opened"));
    };
    // Subscribe before the snapshot so a push landing in between still arrives live.
    let subscription = state
        .notices()
        .subscribe(ws.id(), user.user().id(), user.token_id());
    let repos = state.git().repos();
    let refs = list_refs(repos, &repos.path(&team_slug, &slug))
        .await
        .map_err(|err| {
            tracing::error!(id = ws.id(), error = %err, "listing a workspace's refs failed");
            ApiError::internal("events could not be opened")
        })?;
    // A close that ran before `subscribe` missed this stream, so the access is checked again.
    let signed_out = || ApiError::unauthenticated("invalid or expired token");
    if state.db().token_by_id(user.token_id()).await?.is_none() {
        return Err(signed_out());
    }
    let Some(owner) = state
        .db()
        .user_by_username(user.user().username())
        .await?
        .filter(|owner| !owner.disabled())
    else {
        return Err(signed_out());
    };
    // Admin-ness and the row come from the database now, not from the request-time actor, so
    // a dropped flag or a deleted workspace that closed streams before `subscribe` still counts.
    let still_there = state
        .db()
        .workspace_by_slugs(ws.team_slug(), ws.slug())
        .await?
        .is_some_and(|now| now.id() == ws.id());
    let readable =
        owner.server_admin() || state.db().role_for(ws.id(), owner.id()).await?.is_some();
    if !(still_there && readable) {
        return Err(ApiError::not_found("no such workspace"));
    }
    let first: Vec<Result<Event, Infallible>> =
        std::iter::once(Event::default().retry(Duration::from_secs(2)))
            .chain(
                refs.iter()
                    .filter(|(name, _)| name.starts_with("refs/heads/"))
                    .map(|(name, sha)| ref_event(name, sha)),
            )
            .map(Ok)
            .collect();
    // The subscription lives in the unfold state, so a client hanging up unregisters it.
    let live = stream::unfold(subscription, |mut sub| async move {
        sub.next()
            .await
            .map(|m| (Ok(ref_event(m.ref_name(), m.new_sha())), sub))
    });
    Ok(Sse::new(stream::iter(first).chain(live))
        .keep_alive(keep_alive())
        .into_response())
}

/// For each of `users`, closes the streams on workspaces the user can no longer read: no grant
/// (`role_for` is `None`) and not a server admin. Returns how many streams closed.
pub(crate) async fn reap_unreadable(
    state: &AppState,
    users: &[UserRecord],
) -> Result<usize, ApiError> {
    let notices = state.notices();
    let mut closed = 0;
    for user in users {
        if user.server_admin() {
            continue;
        }
        // Only workspaces the user streams cost a query, so a user without streams costs none.
        for ws in notices.workspaces_of(user.id()) {
            if state.db().role_for(ws, user.id()).await?.is_none() {
                closed += notices.close_grant(ws, user.id());
            }
        }
    }
    Ok(closed)
}

/// `event: ref` with `data: {"ref": <ref>, "sha": <sha>}`.
fn ref_event(ref_name: &str, sha: &str) -> Event {
    let data = serde_json::json!({"ref": ref_name, "sha": sha});
    Event::default().event("ref").data(data.to_string())
}

/// The stream's keep-alive: a comment every 15 s (measurement 1).
pub(crate) fn keep_alive() -> KeepAlive {
    KeepAlive::new().interval(Duration::from_secs(15))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_keep_alive_is_every_15_seconds() {
        let rendered = format!("{:?}", super::keep_alive());
        assert!(rendered.contains("max_interval: 15s"), "{rendered}");
    }
}
