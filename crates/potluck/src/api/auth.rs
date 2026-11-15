//! Turning a presented credential into the request's actor.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use super::credential::{Credential, credential_from_headers};
use super::error::ApiError;
use super::{Actor, AppState, AuthUser};
use crate::auth::token::{digest, digests_match, prefix_of};
use crate::clock::{now_rfc3339, shift_secs};

/// Resolves `credential` to its user and token: a shape check, the rows sharing its prefix, a
/// constant-time digest match against every one, then expiry (`expires_at <= now`) and the
/// owner's `disabled` flag. On success `last_used_at` is refreshed when older than a minute.
/// Every refusal is 401 "invalid or expired token"; only a database failure answers otherwise.
pub async fn authenticate(state: &AppState, credential: &Credential) -> Result<AuthUser, ApiError> {
    let secret = credential.secret();
    let Some(prefix) = prefix_of(secret) else {
        return Err(rejected("none", "malformed"));
    };
    let presented = digest(secret);
    let candidates = state.db().tokens_by_prefix(prefix).await?;
    // Compare against every candidate before choosing, so the time taken does not depend on
    // which row (if any) matched.
    let mut found = None;
    for candidate in candidates {
        if digests_match(candidate.digest(), &presented) && found.is_none() {
            found = Some(candidate);
        }
    }
    let Some(candidate) = found else {
        return Err(rejected(prefix, "unknown"));
    };
    let now = now_rfc3339();
    if candidate
        .token()
        .expires_at()
        .is_some_and(|at| at <= now.as_str())
    {
        return Err(rejected(prefix, "expired"));
    }
    // A disabled owner is refused here, before any `Actor::User` exists to carry their flags.
    if candidate.user().disabled() {
        return Err(rejected(prefix, "disabled"));
    }
    let token_id = candidate.token().id();
    // `last_used_at` is advisory: the request is already authenticated, so a failed touch is
    // reported and ignored.
    if let Some(stale_before) = shift_secs(&now, -60)
        && let Err(err) = state.db().touch_token(token_id, &now, &stale_before).await
    {
        tracing::warn!(error = %err, "could not record token use");
    }
    Ok(AuthUser::new(candidate.user().clone(), token_id))
}

// The one refusal every bad token gets; the log names only the displayable prefix and why.
fn rejected(prefix: &str, reason: &'static str) -> ApiError {
    tracing::debug!(prefix, reason, "token rejected");
    ApiError::unauthenticated("invalid or expired token")
}

/// The request's actor: `System` when the `Extension<Actor>` says so (the admin socket), else
/// the `Authorization` header resolved through `authenticate`, `Anonymous` without one, 401 for
/// a present but unusable or unknown credential. A missing extension counts as `Anonymous`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Authenticated {
    actor: Actor,
}

impl Authenticated {
    /// The resolved actor.
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// The resolved actor, by value.
    pub fn into_actor(self) -> Actor {
        self.actor
    }

    /// The signed-in user: `Anonymous` is 401 "a valid token is required"; `System` is 403
    /// "the system actor has no account".
    pub fn require_user(&self) -> Result<&AuthUser, ApiError> {
        match &self.actor {
            Actor::User(user) => Ok(user),
            Actor::System => Err(ApiError::forbidden("the system actor has no account")),
            Actor::Anonymous => Err(token_required()),
        }
    }

    /// `Anonymous` is 401 "a valid token is required"; a user without `server_admin` is 403
    /// "server admin only"; `System` passes.
    pub fn require_server_admin(&self) -> Result<(), ApiError> {
        match &self.actor {
            Actor::Anonymous => Err(token_required()),
            actor if actor.is_server_admin() => Ok(()),
            _ => Err(ApiError::forbidden("server admin only")),
        }
    }
}

fn token_required() -> ApiError {
    ApiError::unauthenticated("a valid token is required")
}

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = ApiError;

    // The admin socket's actor is trusted outright, so its headers are never parsed; on TCP the
    // header decides, and only its absence is anonymous.
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if let Some(Actor::System) = parts.extensions.get::<Actor>() {
            return Ok(Authenticated {
                actor: Actor::System,
            });
        }
        let actor = match credential_from_headers(&parts.headers)? {
            Some(credential) => Actor::User(authenticate(state, &credential).await?),
            None => Actor::Anonymous,
        };
        Ok(Authenticated { actor })
    }
}
