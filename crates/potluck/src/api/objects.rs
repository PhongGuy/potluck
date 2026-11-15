//! The contract's JSON objects: `User`, `Token`, `Identity`.

use serde::Serialize;

use crate::db::accounts::{TokenRecord, UserRecord};

/// The `User` object: `{username, display_name, email|null, server_admin, disabled, created_at}`.
/// The password hash is never part of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct UserJson {
    username: String,
    display_name: String,
    email: Option<String>,
    server_admin: bool,
    disabled: bool,
    created_at: String,
}

impl From<&UserRecord> for UserJson {
    fn from(user: &UserRecord) -> Self {
        UserJson {
            username: user.username().to_owned(),
            display_name: user.display_name().to_owned(),
            email: user.email().map(str::to_owned),
            server_admin: user.server_admin(),
            disabled: user.disabled(),
            created_at: user.created_at().to_owned(),
        }
    }
}

/// The `Token` object: `{id, name, prefix, created_at, last_used_at|null, expires_at|null}`.
/// The digest is never part of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct TokenJson {
    id: i64,
    name: String,
    prefix: String,
    created_at: String,
    last_used_at: Option<String>,
    expires_at: Option<String>,
}

impl From<&TokenRecord> for TokenJson {
    fn from(token: &TokenRecord) -> Self {
        TokenJson {
            id: token.id(),
            name: token.name().to_owned(),
            prefix: token.prefix().to_owned(),
            created_at: token.created_at().to_owned(),
            last_used_at: token.last_used_at().map(str::to_owned),
            expires_at: token.expires_at().map(str::to_owned),
        }
    }
}

/// The `Identity` object: `{name, email}`, the author and committer of a user's commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct IdentityJson {
    name: String,
    email: String,
}

/// `display_name` (the username when blank) and the email, else
/// `{username}@users.noreply.{host of public_url}`.
pub fn identity(user: &UserRecord, public_url: &str) -> IdentityJson {
    let display = user.display_name().trim();
    let name = if display.is_empty() {
        user.username()
    } else {
        display
    };
    // The public URL is validated at startup; the fallback host only guards against a panic.
    let email = user.email().map_or_else(
        || {
            let parsed = url::Url::parse(public_url).ok();
            let host = parsed
                .as_ref()
                .and_then(url::Url::host_str)
                .unwrap_or("localhost");
            format!("{}@users.noreply.{host}", user.username())
        },
        str::to_owned,
    );
    IdentityJson {
        name: name.to_owned(),
        email,
    }
}
