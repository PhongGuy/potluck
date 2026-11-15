//! The token a client presents in `Authorization`, as Bearer or as the password of Basic.

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::error::ApiError;

/// A token as presented by a client. `Debug` prints `Credential(redacted)` so it can never reach
/// a log through a struct dump.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Credential(String);

impl Credential {
    /// Wraps a token.
    pub fn new(token: String) -> Self {
        Credential(token)
    }

    /// The token text, for hashing and comparison only.
    pub fn secret(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credential(redacted)")
    }
}

/// Reads `Authorization: Bearer <token>` or `Authorization: Basic base64(<anything>:<token>)`.
/// `Ok(None)` when the header is absent; `Err(unauthenticated)` when present but unusable
/// (unknown scheme, bad base64, no colon, empty token). The scheme is case-insensitive.
pub fn credential_from_headers(headers: &HeaderMap) -> Result<Option<Credential>, ApiError> {
    let Some(value) = headers.get(AUTHORIZATION) else {
        return Ok(None);
    };
    // Every malformed shape answers the same fixed text, so the header never echoes back.
    let token = value.to_str().ok().and_then(token_from_value);
    match token {
        Some(token) if !token.is_empty() => Ok(Some(Credential(token))),
        _ => Err(ApiError::unauthenticated("a valid token is required")),
    }
}

/// The token inside one header value, or `None` when the scheme or encoding is unusable.
fn token_from_value(value: &str) -> Option<String> {
    let (scheme, rest) = value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        return Some(rest.to_owned());
    }
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    // Git sends the token as the Basic password; the username is whatever the user typed.
    let decoded = String::from_utf8(STANDARD.decode(rest).ok()?).ok()?;
    let (_user, token) = decoded.split_once(':')?;
    Some(token.to_owned())
}
