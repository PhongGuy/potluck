//! `Provider` against the mock IdP: discovery, the authorization URL, the exchange and every ID
//! token the verifier must refuse.

use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use potluck::oidc::{IdpHttp, Provider, ProviderError};
use sha2::{Digest, Sha256};

use crate::common::idp::{CLIENT_ID, CLIENT_SECRET, Fault, Identity, MockIdp};

const REDIRECT: &str = "http://127.0.0.1:8080/api/v1/oidc/callback";
const VERIFIER: &str = "server-pkce-verifier-0123456789abcdefghijklmnopq";

// The production limits: an unoptimised test build signs RSA slowly, so nothing tight here.
fn http() -> IdpHttp {
    IdpHttp::new(false).unwrap()
}

// A 1 s total timeout, only for the test that makes the IdP hang.
fn impatient_http() -> IdpHttp {
    IdpHttp::with_limits(
        false,
        Duration::from_secs(1),
        Duration::from_secs(1),
        1024 * 1024,
    )
    .unwrap()
}

async fn setup() -> (tempfile::TempDir, MockIdp, Provider) {
    let dir = tempfile::tempdir().unwrap();
    let idp = MockIdp::start().await;
    let provider = Provider::new(idp.settings(dir.path(), false), http());
    (dir, idp, provider)
}

async fn sign_in(
    idp: &MockIdp,
    provider: &Provider,
    now: Instant,
) -> Result<potluck::oidc::IdClaims, ProviderError> {
    let url = provider
        .authorize_url_at(now, REDIRECT, "server-state", "server-nonce", VERIFIER)
        .await?;
    let (code, state) = idp.authorize(&url).await;
    assert_eq!(state, "server-state");
    provider
        .sign_in_at(now, REDIRECT, &code, VERIFIER, "server-nonce")
        .await
}

#[tokio::test]
async fn the_authorization_url_carries_the_flow_parameters() {
    let (_dir, idp, provider) = setup().await;
    let url = provider
        .authorize_url_at(
            Instant::now(),
            REDIRECT,
            "server-state",
            "server-nonce",
            VERIFIER,
        )
        .await
        .unwrap();
    assert!(
        url.starts_with(&format!("{}/authorize?", idp.issuer())),
        "{url}"
    );
    let url = url::Url::parse(&url).unwrap();
    let get = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    assert_eq!(get("response_type").as_deref(), Some("code"));
    assert_eq!(get("client_id").as_deref(), Some(CLIENT_ID));
    assert_eq!(get("redirect_uri").as_deref(), Some(REDIRECT));
    assert_eq!(get("state").as_deref(), Some("server-state"));
    assert_eq!(get("nonce").as_deref(), Some("server-nonce"));
    assert_eq!(get("code_challenge_method").as_deref(), Some("S256"));
    assert_eq!(get("prompt").as_deref(), Some("login"));
    assert_eq!(
        get("code_challenge"),
        Some(URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes())))
    );
    let scopes = get("scope").unwrap();
    for scope in ["openid", "email", "profile"] {
        assert!(scopes.split(' ').any(|s| s == scope), "{scopes}");
    }
    assert!(!url.as_str().contains(CLIENT_SECRET));
}

#[tokio::test]
async fn a_good_sign_in_returns_the_claims() {
    let (_dir, idp, provider) = setup().await;
    idp.set_identity(Identity {
        subject: "abc-123".into(),
        email: Some("ada@example.com".into()),
        preferred_username: Some("ada".into()),
        name: Some("Ada L".into()),
    });
    let claims = sign_in(&idp, &provider, Instant::now()).await.unwrap();
    assert_eq!(claims.subject(), "abc-123");
    assert_eq!(claims.email(), Some("ada@example.com"));
    assert_eq!(claims.preferred_username(), Some("ada"));
    assert_eq!(claims.name(), Some("Ada L"));
    assert_eq!(idp.discovery_calls(), 1, "discovery is cached");
}

#[tokio::test]
async fn every_bad_id_token_is_refused() {
    for fault in [
        Fault::WrongIssuer,
        Fault::WrongAudience,
        Fault::Expired,
        Fault::WrongNonce,
        Fault::AlgNone,
    ] {
        let (_dir, idp, provider) = setup().await;
        let url = provider
            .authorize_url_at(Instant::now(), REDIRECT, "s", "server-nonce", VERIFIER)
            .await
            .unwrap();
        let (code, _) = idp.authorize(&url).await;
        idp.set_fault(fault);
        let err = provider
            .sign_in_at(Instant::now(), REDIRECT, &code, VERIFIER, "server-nonce")
            .await
            .unwrap_err();
        assert_eq!(err, ProviderError::Refused, "{fault:?}");
    }
}

#[tokio::test]
async fn a_failed_exchange_is_refused() {
    let (_dir, idp, provider) = setup().await;
    let now = Instant::now();
    provider
        .authorize_url_at(now, REDIRECT, "s", "n", VERIFIER)
        .await
        .unwrap();
    let err = provider
        .sign_in_at(now, REDIRECT, "never-issued", VERIFIER, "n")
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::Refused);
    let url = provider
        .authorize_url_at(now, REDIRECT, "s", "n", VERIFIER)
        .await
        .unwrap();
    let (code, _) = idp.authorize(&url).await;
    let err = provider
        .sign_in_at(
            now,
            REDIRECT,
            &code,
            "another-verifier-0123456789abcdefghijk",
            "n",
        )
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::Refused, "wrong PKCE verifier");
}

#[tokio::test]
async fn an_unreachable_provider_is_unavailable_at_the_url_step() {
    for fault in [
        Fault::Unavailable,
        Fault::Redirect,
        Fault::Oversized,
        Fault::Hang,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let idp = MockIdp::start().await;
        let provider = Provider::new(idp.settings(dir.path(), false), impatient_http());
        idp.set_fault(fault);
        let started = Instant::now();
        let err = provider
            .authorize_url_at(Instant::now(), REDIRECT, "s", "n", VERIFIER)
            .await
            .unwrap_err();
        assert_eq!(err, ProviderError::Unavailable, "{fault:?}");
        assert!(started.elapsed() < Duration::from_secs(5), "{fault:?}");
    }
}

#[tokio::test]
async fn a_token_endpoint_outage_is_refused() {
    let (_dir, idp, provider) = setup().await;
    let now = Instant::now();
    let url = provider
        .authorize_url_at(now, REDIRECT, "s", "n", VERIFIER)
        .await
        .unwrap();
    let (code, _) = idp.authorize(&url).await;
    idp.set_fault(Fault::Unavailable);
    assert_eq!(
        provider
            .sign_in_at(now, REDIRECT, &code, VERIFIER, "n")
            .await
            .unwrap_err(),
        ProviderError::Refused
    );
}

#[tokio::test]
async fn an_issuer_differing_by_a_trailing_slash_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let idp = MockIdp::start().await;
    let file = dir.path().join("secret");
    std::fs::write(&file, CLIENT_SECRET).unwrap();
    let issuer = format!("{}/", idp.issuer());
    let settings = potluck::oidc::OidcSettings::from_flags(
        Some(&issuer),
        Some(CLIENT_ID),
        Some(&file),
        None,
        false,
        false,
    )
    .unwrap()
    .unwrap();
    let provider = Provider::new(settings, http());
    assert_eq!(
        provider
            .authorize_url_at(Instant::now(), REDIRECT, "s", "n", VERIFIER)
            .await
            .unwrap_err(),
        ProviderError::Unavailable
    );
}
