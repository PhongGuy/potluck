//! Roles order; credentials parse from both schemes and never print themselves.

use axum::http::{HeaderMap, HeaderValue};
use base64::Engine;
use potluck::api::credential::{Credential, credential_from_headers};
use potluck::api::error::ErrorCode;
use potluck::role::Role;

fn headers(value: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("authorization", HeaderValue::from_str(value).unwrap());
    h
}

#[test]
fn roles_order_read_write_admin() {
    assert!(Role::Read < Role::Write && Role::Write < Role::Admin);
    assert_eq!(Role::Write.as_str(), "write");
    assert!(Role::Admin >= Role::Write, "admin may push");
    assert!(Role::Read < Role::Write, "read may not push");
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

#[test]
fn debug_never_prints_the_token() {
    let c = Credential::new("plk_supersecret".into());
    let shown = format!("{c:?}");
    assert!(!shown.contains("supersecret"), "{shown}");
    assert!(shown.contains("redacted"), "{shown}");
}
