//! `POST /session`: password sign-in that mints a personal access token.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::AppState;
use super::client_ip::ClientIp;
use super::error::{ApiError, ApiResult};
use super::json::ApiJson;
use super::objects::{TokenJson, UserJson};
use super::{tokens, validate};
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
    token: String,
    token_meta: TokenJson,
    user: UserJson,
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

    // Every credential failure gets the same refusal; only the username reaches the log.
    let refused = || {
        tracing::debug!(username = %body.username, "sign-in refused");
        count(SignInOutcome::Failure);
        ApiError::unauthenticated("invalid username or password")
    };
    let Some(user) = state.db().user_by_username(&body.username).await? else {
        return Err(refused().into());
    };
    let Some(phc) = user.password_hash().filter(|_| !user.disabled()) else {
        return Err(refused().into());
    };
    // argon2 is deliberately slow, so it runs off the async workers.
    let passwords = state.passwords().clone();
    let (password, phc) = (body.password, phc.to_owned());
    let verified = tokio::task::spawn_blocking(move || passwords.verify(&password, &phc))
        .await
        .map_err(|_| ApiError::internal("password check failed"))?;
    match verified {
        Ok(true) => {}
        Ok(false) => return Err(refused().into()),
        Err(_) => {
            tracing::error!(username = %body.username, "stored password hash is unusable");
            return Err(ApiError::internal("password check failed").into());
        }
    }

    let (token, record) = tokens::mint_and_create(&state, user.id(), token_name, None).await?;
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
