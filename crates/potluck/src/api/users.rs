//! User management: `GET /users` and `POST /users` for server admins, `PATCH /users/{username}`
//! for admins and the user themself.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Deserializer};

use super::AppState;
use super::auth::Authenticated;
use super::error::ApiError;
use super::events;
use super::json::ApiJson;
use super::objects::UserJson;
use crate::db::accounts::{NewUser, UserPatch};
use crate::slug::Slug;

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

/// The `PATCH /users/{username}` body; an absent field is left alone. `email: null` clears the
/// email. No `Debug`, so neither password can reach a log by formatting.
#[derive(Deserialize)]
pub(super) struct PatchUser {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    email: Option<Option<String>>,
    #[serde(default)]
    server_admin: Option<bool>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    current_password: Option<String>,
}

// A present field, `null` included, is `Some`; only an absent one stays `None` via the default.
fn double_option<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
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

/// `PATCH /users/{username}`, any of `display_name`, `email` (nullable), `server_admin`,
/// `disabled`, `password`, `current_password`. A server admin or `System` may set any field and
/// needs no `current_password` for another user. The user themself may set `display_name`,
/// `email` and `password`; `password` requires a matching `current_password` (missing or wrong:
/// 401), and each verification counts against the per-username session limiter (429 when full);
/// `server_admin` or `disabled` in a self patch by a non-admin is 403. A non-admin patching
/// another user, or anyone patching an unknown user, gets 404. The same value rules as `create`
/// (422), and display name and email are stored trimmed. `{}` is 200 with the unchanged user.
pub(super) async fn patch(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(username): Path<String>,
    ApiJson(body): ApiJson<PatchUser>,
) -> Result<Json<UserJson>, ApiError> {
    // Admins reach anyone; everyone else only themself, and another user is not even readable.
    let admin = auth.actor().is_server_admin();
    let caller = if admin {
        auth.actor().user()
    } else {
        Some(auth.require_user()?)
    };
    let is_self = caller.is_some_and(|user| user.user().username() == username);
    if !admin {
        if !is_self {
            return Err(ApiError::not_found("no such user"));
        }
        if body.server_admin.is_some() || body.disabled.is_some() {
            return Err(ApiError::forbidden(
                "only a server admin may change server_admin or disabled",
            ));
        }
    }
    let Some(target) = state.db().user_by_username(&username).await? else {
        return Err(ApiError::not_found("no such user"));
    };

    if let Some(name) = &body.display_name {
        check_display_name(name)?;
    }
    if let Some(Some(email)) = &body.email {
        check_email(email)?;
    }
    if let Some(password) = &body.password {
        check_password(password)?;
    }

    let mut patch = UserPatch::default();
    if let Some(password) = body.password {
        // Changing one's own password proves the current one, rate limited like a sign-in.
        if is_self {
            let Some(current) = body.current_password else {
                return Err(ApiError::unauthenticated("current_password is required"));
            };
            state
                .session_limits()
                .per_username()
                .check(&username)
                .map_err(ApiError::rate_limited)?;
            let Some(phc) = target.password_hash().map(str::to_owned) else {
                return Err(ApiError::unauthenticated("current_password is wrong"));
            };
            let passwords = state.passwords().clone();
            let verified = tokio::task::spawn_blocking(move || passwords.verify(&current, &phc))
                .await
                .map_err(|_| ApiError::internal("password check failed"))?
                .map_err(|_| ApiError::internal("password check failed"))?;
            if !verified {
                return Err(ApiError::unauthenticated("current_password is wrong"));
            }
        }
        let passwords = state.passwords().clone();
        let phc = tokio::task::spawn_blocking(move || passwords.hash(&password))
            .await
            .map_err(|_| ApiError::internal("password hashing failed"))?
            .map_err(|_| ApiError::internal("password hashing failed"))?;
        patch = patch.password_hash(&phc);
    }
    if let Some(name) = &body.display_name {
        patch = patch.display_name(name.trim());
    }
    if let Some(email) = &body.email {
        patch = patch.email(email.as_deref().map(str::trim));
    }
    if let Some(yes) = body.server_admin {
        patch = patch.server_admin(yes);
    }
    if let Some(yes) = body.disabled {
        patch = patch.disabled(yes);
    }
    let Some(user) = state.db().update_user(&username, patch).await? else {
        return Err(ApiError::not_found("no such user"));
    };
    if body.disabled == Some(true) {
        state.notices().close_user(user.id());
    }
    // The implicit admin role was never a grant row, so streams that relied on it close.
    // The patch is committed either way; a failed reap is logged rather than answered as 500.
    if body.server_admin == Some(false)
        && let Err(err) = events::reap_unreadable(&state, std::slice::from_ref(&user)).await
    {
        tracing::error!(user = user.id(), error = ?err, "closing streams after an admin drop failed");
    }
    Ok(Json(UserJson::from(&user)))
}

/// `^[a-z0-9][a-z0-9-]{0,62}$` and not the reserved `system`.
pub(super) fn check_username(s: &str) -> Result<(), ApiError> {
    if Slug::parse(s).is_ok() && s != "system" {
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
