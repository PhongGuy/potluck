//! User management by server admins: `GET /users` and `POST /users`.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;

use super::AppState;
use super::auth::Authenticated;
use super::error::ApiError;
use super::json::ApiJson;
use super::objects::UserJson;
use crate::db::accounts::NewUser;

/// The `POST /users` body. No `Debug`, so the password cannot reach a log by formatting.
#[derive(Deserialize)]
pub(super) struct CreateUser {
    username: String,
    display_name: String,
    email: Option<String>,
    password: String,
    #[serde(default)]
    server_admin: bool,
}

/// `GET /users` (server admin or `System`): every user by username.
pub(super) async fn list(
    State(state): State<AppState>,
    auth: Authenticated,
) -> Result<Json<Vec<UserJson>>, ApiError> {
    auth.require_server_admin()?;
    let users = state.db().list_users().await?;
    Ok(Json(users.iter().map(UserJson::from).collect()))
}

/// `POST /users` (server admin or `System`) `{username, display_name, email?, password,
/// server_admin?}`: 422 naming the first invalid field, 409 when the username is taken, else
/// 201 `User`. Display name and email are stored trimmed.
pub(super) async fn create(
    State(state): State<AppState>,
    auth: Authenticated,
    ApiJson(body): ApiJson<CreateUser>,
) -> Result<(StatusCode, Json<UserJson>), ApiError> {
    auth.require_server_admin()?;
    // The whole body is checked before hashing, so a refusal costs no argon2 work.
    check_username(&body.username)?;
    check_display_name(&body.display_name)?;
    if let Some(email) = &body.email {
        check_email(email)?;
    }
    check_password(&body.password)?;

    let passwords = state.passwords().clone();
    let password = body.password;
    let phc = tokio::task::spawn_blocking(move || passwords.hash(&password))
        .await
        .map_err(|_| ApiError::internal("password hashing failed"))?
        .map_err(|_| ApiError::internal("password hashing failed"))?;

    let mut new = NewUser::new(&body.username, body.display_name.trim())
        .password_hash(&phc)
        .server_admin(body.server_admin);
    if let Some(email) = &body.email {
        new = new.email(email.trim());
    }
    let user = state.db().create_user(new).await?;
    Ok((StatusCode::CREATED, Json(UserJson::from(&user))))
}

/// `^[a-z0-9][a-z0-9-]{0,62}$` and not the reserved `system`.
pub(super) fn check_username(s: &str) -> Result<(), ApiError> {
    let bytes = s.as_bytes();
    let valid = matches!(bytes.first(), Some(b) if b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.len() <= 63
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && s != "system";
    if valid {
        Ok(())
    } else {
        Err(ApiError::invalid_value(
            "username",
            "username must be 1 to 63 lowercase letters, digits or hyphens, start with a letter \
             or digit, and not be system",
        ))
    }
}

/// 1 to 200 characters after trimming.
pub(super) fn check_display_name(s: &str) -> Result<(), ApiError> {
    let len = s.trim().chars().count();
    if (1..=200).contains(&len) {
        Ok(())
    } else {
        Err(ApiError::invalid_value(
            "display_name",
            "display_name must be 1 to 200 characters",
        ))
    }
}

/// At most 254 characters after trimming, with an `@` between non-empty parts.
pub(super) fn check_email(s: &str) -> Result<(), ApiError> {
    let s = s.trim();
    let shaped = s
        .split_once('@')
        .is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty());
    if shaped && s.chars().count() <= 254 {
        Ok(())
    } else {
        Err(ApiError::invalid_value(
            "email",
            "email must be at most 254 characters with an @ between non-empty parts",
        ))
    }
}

/// 8 to 1024 bytes, the argon2 input as given.
pub(super) fn check_password(s: &str) -> Result<(), ApiError> {
    if (8..=1024).contains(&s.len()) {
        Ok(())
    } else {
        Err(ApiError::invalid_value(
            "password",
            "password must be 8 to 1024 bytes",
        ))
    }
}
