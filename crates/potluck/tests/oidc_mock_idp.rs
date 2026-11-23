//! The mock IdP itself: what it publishes and what its token endpoint accepts.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::common::idp::{CLIENT_ID, CLIENT_SECRET, Fault, MockIdp};

async fn get_json(url: &str) -> Value {
    reqwest::get(url).await.unwrap().json().await.unwrap()
}

fn authorize_url(idp: &MockIdp, verifier: &str) -> String {
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    format!(
        "{}/authorize?response_type=code&client_id={CLIENT_ID}&redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Fcb&state=st&nonce=nn&code_challenge={challenge}&code_challenge_method=S256",
        idp.issuer()
    )
}

async fn token(idp: &MockIdp, code: &str, verifier: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/token", idp.issuer()))
        .basic_auth(CLIENT_ID, Some(CLIENT_SECRET))
        // reqwest's `form` feature is off in this workspace, so the body is encoded by hand.
        .header("content-type", "application/x-www-form-urlencoded")
        .body(
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("grant_type", "authorization_code")
                .append_pair("code", code)
                .append_pair("redirect_uri", "http://127.0.0.1:9/cb")
                .append_pair("code_verifier", verifier)
                .finish(),
        )
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn publishes_discovery_and_one_key_at_a_time() {
    let idp = MockIdp::start().await;
    let doc = get_json(&format!(
        "{}/.well-known/openid-configuration",
        idp.issuer()
    ))
    .await;
    assert_eq!(doc["issuer"], idp.issuer());
    let keys = get_json(&format!("{}/jwks", idp.issuer())).await;
    assert_eq!(keys["keys"][0]["kid"], "k1");
    assert_eq!(keys["keys"].as_array().unwrap().len(), 1);
    idp.rotate_key();
    let keys = get_json(&format!("{}/jwks", idp.issuer())).await;
    assert_eq!(keys["keys"][0]["kid"], "k2");
    assert_eq!(idp.discovery_calls(), 1);
    assert_eq!(idp.jwks_calls(), 2);
}

#[tokio::test]
async fn redeems_a_code_once_with_the_right_verifier() {
    let idp = MockIdp::start().await;
    let (code, state) = idp.authorize(&authorize_url(&idp, "verifier-1")).await;
    assert_eq!(state, "st");
    assert_eq!(token(&idp, &code, "wrong").await.status(), 400);
    let (code, _) = idp.authorize(&authorize_url(&idp, "verifier-1")).await;
    let ok = token(&idp, &code, "verifier-1").await;
    assert_eq!(ok.status(), 200);
    let body: Value = ok.json().await.unwrap();
    assert_eq!(body["id_token"].as_str().unwrap().split('.').count(), 3);
    assert_eq!(
        token(&idp, &code, "verifier-1").await.status(),
        400,
        "codes are single-use"
    );
}

#[tokio::test]
async fn transport_faults_hit_every_endpoint() {
    let idp = MockIdp::start().await;
    idp.set_fault(Fault::Unavailable);
    for path in ["/.well-known/openid-configuration", "/jwks"] {
        let status = reqwest::get(format!("{}{path}", idp.issuer()))
            .await
            .unwrap()
            .status();
        assert_eq!(status, 503, "{path}");
    }
}
