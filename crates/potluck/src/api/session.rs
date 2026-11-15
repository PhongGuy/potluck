//! `POST /session`: password sign-in that mints a personal access token.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::AppState;
use super::client_ip::ClientIp;
use super::error::ApiError;
use super::json::ApiJson;
use super::objects::{TokenJson, UserJson};
use crate::auth::token::MintedToken;
use crate::db::accounts::NewToken;

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
) -> Result<(StatusCode, Json<SessionResponse>), ApiError> {
    let token_name = body.token_name.trim();
    if token_name.is_empty() || token_name.chars().count() > 100 {
        return Err(ApiError::invalid_value(
            "token_name",
            "token_name must be 1 to 100 characters",
        ));
    }
    // Only attempts that reach the credential check count, the address before the username.
    let limits = state.session_limits();
    limits
        .per_ip()
        .check(&ip.key())
        .map_err(ApiError::rate_limited)?;
    limits
        .per_username()
        .check(&body.username)
        .map_err(ApiError::rate_limited)?;

    // Every credential failure gets the same refusal; only the username reaches the log.
    let refused = || {
        tracing::debug!(username = %body.username, "sign-in refused");
        ApiError::unauthenticated("invalid username or password")
    };
    let Some(user) = state.db().user_by_username(&body.username).await? else {
        return Err(refused());
    };
    let Some(phc) = user.password_hash().filter(|_| !user.disabled()) else {
        return Err(refused());
    };
    // argon2 is deliberately slow, so it runs off the async workers.
    let passwords = state.passwords().clone();
    let (password, phc) = (body.password, phc.to_owned());
    let verified = tokio::task::spawn_blocking(move || passwords.verify(&password, &phc))
        .await
        .map_err(|_| ApiError::internal("password check failed"))?;
    match verified {
        Ok(true) => {}
        Ok(false) => return Err(refused()),
        Err(_) => {
            tracing::error!(username = %body.username, "stored password hash is unusable");
            return Err(ApiError::internal("password check failed"));
        }
    }

    let minted = MintedToken::mint().map_err(|_| ApiError::internal("could not mint a token"))?;
    let record = state
        .db()
        .create_token(NewToken::new(
            user.id(),
            token_name,
            minted.prefix(),
            *minted.digest(),
            None,
        ))
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(SessionResponse {
            token: minted.secret().to_owned(),
            token_meta: TokenJson::from(&record),
            user: UserJson::from(&user),
        }),
    ))
}
