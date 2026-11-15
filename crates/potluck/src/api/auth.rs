//! Turning a presented credential into the request's actor.

use super::credential::Credential;
use super::error::ApiError;
use super::{AppState, AuthUser};
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
