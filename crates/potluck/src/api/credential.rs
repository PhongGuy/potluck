//! The token a client presents in `Authorization`, as Bearer or as the password of Basic.

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::error::ApiError;

/// A token as presented by a client. `Debug` prints `Credential(redacted)` so it can never reach
/// a log through a struct dump.
#[derive(Clone)]
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
pub(crate) fn credential_from_headers(headers: &HeaderMap) -> Result<Option<Credential>, ApiError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::error::ErrorCode;
    use axum::http::HeaderValue;
    use base64::Engine;

    fn headers(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn bearer_and_basic_yield_the_token() {
        let bearer = credential_from_headers(&headers("Bearer plk_abc"))
            .unwrap()
            .unwrap();
        assert_eq!(bearer.secret(), "plk_abc");
        let basic = base64::engine::general_purpose::STANDARD.encode("anyone:plk_abc");
        let from_basic = credential_from_headers(&headers(&format!("Basic {basic}")))
            .unwrap()
            .unwrap();
        assert_eq!(
            from_basic.secret(),
            bearer.secret(),
            "the username is ignored"
        );
        let lower = credential_from_headers(&headers("bearer plk_abc"))
            .unwrap()
            .unwrap();
        assert_eq!(
            lower.secret(),
            bearer.secret(),
            "schemes are case-insensitive"
        );
        let colon = base64::engine::general_purpose::STANDARD.encode("user:plk_a:b");
        assert_eq!(
            credential_from_headers(&headers(&format!("Basic {colon}")))
                .unwrap()
                .unwrap()
                .secret(),
            "plk_a:b",
            "split at the first colon"
        );
    }

    #[test]
    fn absent_is_none_and_malformed_is_401() {
        assert!(
            credential_from_headers(&HeaderMap::new())
                .unwrap()
                .is_none()
        );
        let no_colon = base64::engine::general_purpose::STANDARD.encode("justatoken");
        let empty_pw = base64::engine::general_purpose::STANDARD.encode("user:");
        for bad in [
            "Digest abc",
            "Bearer",
            "Bearer ",
            "Basic !!!",
            &format!("Basic {no_colon}"),
            &format!("Basic {empty_pw}"),
        ] {
            let err = credential_from_headers(&headers(bad))
                .err()
                .unwrap_or_else(|| panic!("{bad:?} accepted"));
            assert_eq!(err.status(), 401, "{bad:?}");
            assert_eq!(err.code(), ErrorCode::Unauthenticated, "{bad:?}");
        }
    }
}
