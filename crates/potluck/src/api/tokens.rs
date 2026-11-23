//! A user's own tokens, a server admin's view of anyone's, and `GET /me`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::AppState;
use super::audit;
use super::auth::Authenticated;
use super::client_ip::ClientIp;
use super::error::{ApiError, ApiResult};
use super::json::ApiJson;
use super::objects::{IdentityJson, TokenJson, UserJson, WorkspaceJson, identity};
use super::{validate, workspaces};
use crate::auth::token::MintedToken;
use crate::clock::{normalize_rfc3339, now_rfc3339};
use crate::db::accounts::{NewToken, TokenOrigin, TokenRecord};
use crate::db::audit::AuditCtx;

/// The signed-in user, their commit identity and the workspaces they can reach.
#[derive(Serialize)]
pub(super) struct MeResponse {
    user: UserJson,
    identity: IdentityJson,
    workspaces: Vec<WorkspaceJson>,
}

/// The `POST /tokens` body; an absent `expires_at` reads as `None` like an explicit null.
#[derive(Deserialize)]
pub(super) struct CreateToken {
    name: String,
    expires_at: Option<String>,
}

/// The minted secret, shown this once, and its metadata.
#[derive(Serialize)]
pub(super) struct CreatedToken {
    token: String,
    token_meta: TokenJson,
}

/// `GET /me` (self): `{user, identity, workspaces}`, the workspaces as `GET /workspaces` lists them.
pub(super) async fn me(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<MeResponse>> {
    let user = auth.require_user()?.user();
    Ok(Json(MeResponse {
        user: UserJson::from(user),
        identity: identity(user, state.config().public_url()),
        workspaces: workspaces::held(&state, auth.actor()).await?,
    }))
}

/// `GET /tokens` (self): `[Token]` in creation order.
pub(super) async fn list(
    State(state): State<AppState>,
    auth: Authenticated,
) -> ApiResult<Json<Vec<TokenJson>>> {
    let user_id = auth.require_user()?.user().id();
    Ok(token_list(&state, user_id).await?)
}

/// `POST /tokens` (self) `{name, expires_at?}`: `name` 1..=100 chars trimmed (422 `name`);
/// `expires_at` absent or null never expires, else RFC 3339 normalised and after now (422
/// `expires_at`); 201 `{token, token_meta}`.
pub(super) async fn create(
    State(state): State<AppState>,
    auth: Authenticated,
    ip: ClientIp,
    ApiJson(body): ApiJson<CreateToken>,
) -> ApiResult<(StatusCode, Json<CreatedToken>)> {
    let user_id = auth.require_user()?.user().id();
    let name = validate::token_name("name", &body.name)?;
    // Normalised stamps compare chronologically as strings, so "after now" is a string compare.
    let expires_at = match body.expires_at.as_deref() {
        None => None,
        Some(raw) => match normalize_rfc3339(raw) {
            Some(at) if at > now_rfc3339() => Some(at),
            _ => {
                return Err(ApiError::invalid_value(
                    "expires_at",
                    "expires_at must be an RFC 3339 time in the future",
                )
                .into());
            }
        },
    };
    let (token, record) = mint_and_create(
        &state,
        user_id,
        name,
        expires_at.as_deref(),
        TokenOrigin::Api,
        &audit::ctx(&auth, ip),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedToken {
            token,
            token_meta: TokenJson::from(&record),
        }),
    ))
}

/// Mints a token for `user_id` named `name` with `expires_at`, stores its digest with the audit
/// row `origin` names for `ctx`, and returns the secret (shown once) with the stored row; 500
/// "could not mint a token" if minting fails.
pub(super) async fn mint_and_create(
    state: &AppState,
    user_id: i64,
    name: &str,
    expires_at: Option<&str>,
    origin: TokenOrigin,
    ctx: &AuditCtx,
) -> Result<(String, TokenRecord), ApiError> {
    let minted = MintedToken::mint().map_err(|_| ApiError::internal("could not mint a token"))?;
    let record = state
        .db()
        .create_token(
            NewToken::new(user_id, name, minted.prefix(), *minted.digest(), expires_at),
            origin,
            ctx,
        )
        .await?;
    Ok((minted.secret().to_owned(), record))
}

/// `DELETE /tokens/{id}`: 204 for the caller's own token, or any token for a server admin or
/// `System`; 404 for a missing, non-numeric or not-owned id.
pub(super) async fn revoke(
    State(state): State<AppState>,
    auth: Authenticated,
    ip: ClientIp,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    // A server admin may revoke anything; anyone else only their own, and others' tokens answer
    // like missing ones so their ids are not confirmed.
    let owner = if auth.actor().is_server_admin() {
        None
    } else {
        Some(auth.require_user()?.user().id())
    };
    let missing = || ApiError::not_found("no such token");
    let id = id.parse::<i64>().map_err(|_| missing())?;
    let token = state.db().token_by_id(id).await?.ok_or_else(missing)?;
    if owner.is_some_and(|owner| owner != token.user_id()) {
        return Err(missing().into());
    }
    if state.db().delete_token(id, &audit::ctx(&auth, ip)).await? {
        state.notices().close_token(id);
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(missing().into())
    }
}

/// `GET /users/{username}/tokens` (server admin or `System`): `[Token]`; 404 unknown user.
pub(super) async fn list_for_user(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(username): Path<String>,
) -> ApiResult<Json<Vec<TokenJson>>> {
    auth.require_server_admin()?;
    let user = state
        .db()
        .user_by_username(&username)
        .await?
        .ok_or_else(|| ApiError::not_found("no such user"))?;
    Ok(token_list(&state, user.id()).await?)
}

async fn token_list(state: &AppState, user_id: i64) -> Result<Json<Vec<TokenJson>>, ApiError> {
    let tokens = state.db().tokens_for_user(user_id).await?;
    Ok(Json(tokens.iter().map(TokenJson::from).collect()))
}
