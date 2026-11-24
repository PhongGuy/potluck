//! `POST /session`: password sign-in that mints a personal access token.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::AppState;
use super::client_ip::ClientIp;
use super::error::{ApiError, ApiResult, ErrorResponse};
use super::json::ApiJson;
use super::objects::{TokenJson, UserJson};
use super::{tokens, validate};
use crate::db::accounts::TokenOrigin;
use crate::db::audit::{AuditCtx, SignInFailure};
use crate::metrics::{SignInMethod, SignInOutcome};

/// The sign-in body. No `Debug`, so the password cannot reach a log by formatting.
#[derive(Deserialize)]
pub(super) struct SessionRequest {
    username: String,
    password: String,
    token_name: String,
}

/// The minted secret, shown this once, its metadata and the signed-in user.
#[derive(Serialize)]
pub(super) struct SessionResponse {
    pub(super) token: String,
    pub(super) token_meta: TokenJson,
    pub(super) user: UserJson,
}

/// `POST /api/v1/session` `{username, password, token_name}`: `token_name` 1..=100 chars after
/// trimming (422 `token_name`); 429 for the client address, then for the username; 401 "invalid
/// username or password" for an unknown or disabled user, a user without a password, or a wrong
/// password; else 201 `{token, token_meta, user}` with a never-expiring token.
pub(super) async fn create(
    State(state): State<AppState>,
    ip: ClientIp,
    ApiJson(body): ApiJson<SessionRequest>,
) -> ApiResult<(StatusCode, Json<SessionResponse>)> {
    let token_name = validate::token_name("token_name", &body.token_name)?;
    // Only attempts that reach the credential check count, the address before the username.
    let limits = state.session_limits();
    // Each outcome is counted where it is decided; the 422 and an unusable hash count nothing.
    let count = |outcome| state.metrics().sign_in(SignInMethod::Password, outcome);
    limits.per_ip().check(&ip.key()).map_err(|e| {
        count(SignInOutcome::RateLimitedIp);
        ApiError::rate_limited(e)
    })?;
    limits.per_username().check(&body.username).map_err(|e| {
        count(SignInOutcome::RateLimitedUsername);
        ApiError::rate_limited(e)
    })?;

    // Every credential failure gets the same refusal, checked in the order of the stored reason.
    let Some(user) = state.db().user_by_username(&body.username).await? else {
        return Err(refuse(&state, ip, &body.username, None, SignInFailure::UnknownUser).await);
    };
    let known = Some((user.id(), user.username()));
    if user.disabled() {
        return Err(refuse(&state, ip, &body.username, known, SignInFailure::Disabled).await);
    }
    let Some(phc) = user.password_hash() else {
        return Err(refuse(&state, ip, &body.username, known, SignInFailure::NoPassword).await);
    };
    // argon2 is deliberately slow, so it runs off the async workers.
    let passwords = state.passwords().clone();
    let (password, phc) = (body.password, phc.to_owned());
    let verified = tokio::task::spawn_blocking(move || passwords.verify(&password, &phc))
        .await
        .map_err(|_| ApiError::internal("password check failed"))?;
    match verified {
        Ok(true) => {}
        Ok(false) => {
            return Err(refuse(
                &state,
                ip,
                &body.username,
                known,
                SignInFailure::WrongPassword,
            )
            .await);
        }
        Err(_) => {
            tracing::error!(username = %body.username, "stored password hash is unusable");
            return Err(ApiError::internal("password check failed").into());
        }
    }

    // The sign-in is the new token owner's act, not the anonymous request's.
    let ctx = AuditCtx::user(user.id(), user.username());
    let ctx = match ip.addr() {
        Some(addr) => ctx.with_client_ip(addr),
        None => ctx,
    };
    let (token, record) = tokens::mint_and_create(
        &state,
        user.id(),
        token_name,
        None,
        TokenOrigin::Session,
        &ctx,
    )
    .await?;
    count(SignInOutcome::Success);
    Ok((
        StatusCode::CREATED,
        Json(SessionResponse {
            token,
            token_meta: TokenJson::from(&record),
            user: UserJson::from(&user),
        }),
    ))
}

/// Counts and records the refusal and returns the 401. A failed insert is logged and answered the
/// same way, so the log's health never shows in the response.
async fn refuse(
    state: &AppState,
    ip: ClientIp,
    username: &str,
    user: Option<(i64, &str)>,
    reason: SignInFailure,
) -> ErrorResponse {
    tracing::debug!(username = %username, "sign-in refused");
    state
        .metrics()
        .sign_in(SignInMethod::Password, SignInOutcome::Failure);
    if let Err(e) = state
        .db()
        .record_failed_sign_in(user, reason, ip.addr())
        .await
    {
        tracing::error!(error = %e, "could not record a refused sign-in");
    }
    ApiError::unauthenticated("invalid username or password").into()
}
