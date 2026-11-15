//! A user's own tokens, a server admin's view of anyone's, and `GET /me`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::AppState;
use super::auth::Authenticated;
use super::error::ApiError;
use super::json::ApiJson;
use super::objects::{IdentityJson, TokenJson, UserJson, WorkspaceJson, identity};
use super::workspaces;
use crate::auth::token::MintedToken;
use crate::clock::{normalize_rfc3339, now_rfc3339};
use crate::db::accounts::NewToken;

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
) -> Result<Json<MeResponse>, ApiError> {
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
) -> Result<Json<Vec<TokenJson>>, ApiError> {
    let user_id = auth.require_user()?.user().id();
    token_list(&state, user_id).await
}

/// `POST /tokens` (self) `{name, expires_at?}`: `name` 1..=100 chars trimmed (422 `name`);
/// `expires_at` absent or null never expires, else RFC 3339 normalised and after now (422
/// `expires_at`); 201 `{token, token_meta}`.
pub(super) async fn create(
    State(state): State<AppState>,
    auth: Authenticated,
    ApiJson(body): ApiJson<CreateToken>,
) -> Result<(StatusCode, Json<CreatedToken>), ApiError> {
    let user_id = auth.require_user()?.user().id();
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(ApiError::invalid_value(
            "name",
            "name must be 1 to 100 characters",
        ));
    }
    // Normalised stamps compare chronologically as strings, so "after now" is a string compare.
    let expires_at = match body.expires_at.as_deref() {
        None => None,
        Some(raw) => match normalize_rfc3339(raw) {
            Some(at) if at > now_rfc3339() => Some(at),
            _ => {
                return Err(ApiError::invalid_value(
                    "expires_at",
                    "expires_at must be an RFC 3339 time in the future",
                ));
            }
        },
    };
    let minted = MintedToken::mint().map_err(|_| ApiError::internal("could not mint a token"))?;
    let record = state
        .db()
        .create_token(NewToken::new(
            user_id,
            name,
            minted.prefix(),
            *minted.digest(),
            expires_at.as_deref(),
        ))
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedToken {
            token: minted.secret().to_owned(),
            token_meta: TokenJson::from(&record),
        }),
    ))
}

/// `DELETE /tokens/{id}`: 204 for the caller's own token, or any token for a server admin or
/// `System`; 404 for a missing, non-numeric or not-owned id.
pub(super) async fn revoke(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
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
        return Err(missing());
    }
    if state.db().delete_token(id).await? {
        state.notices().close_token(id);
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(missing())
    }
}

/// `GET /users/{username}/tokens` (server admin or `System`): `[Token]`; 404 unknown user.
pub(super) async fn list_for_user(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(username): Path<String>,
) -> Result<Json<Vec<TokenJson>>, ApiError> {
    auth.require_server_admin()?;
    let user = state
        .db()
        .user_by_username(&username)
        .await?
        .ok_or_else(|| ApiError::not_found("no such user"))?;
    token_list(&state, user.id()).await
}

async fn token_list(state: &AppState, user_id: i64) -> Result<Json<Vec<TokenJson>>, ApiError> {
    let tokens = state.db().tokens_for_user(user_id).await?;
    Ok(Json(tokens.iter().map(TokenJson::from).collect()))
}
