//! A mock OpenID provider on 127.0.0.1: discovery, JWKS, authorize and token endpoints. It signs
//! ID tokens with RS256 fixture keys and can be switched into each failure Potluck must refuse.

// Shared test support: each test file uses a different subset of the faults and accessors.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use openidconnect::core::{CoreJwsSigningAlgorithm, CoreRsaPrivateSigningKey};
use openidconnect::{JsonWebKeyId, PrivateSigningKey};
use potluck::oidc::OidcSettings;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const CLIENT_ID: &str = "potluck-test";
pub const CLIENT_SECRET: &str = "idp-test-secret";
const KEYS: [(&str, &str); 2] = [
    ("k1", include_str!("../fixtures/oidc/rsa-1.pem")),
    ("k2", include_str!("../fixtures/oidc/rsa-2.pem")),
];

/// What the mock gets wrong on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    None,
    WrongIssuer,
    WrongAudience,
    Expired,
    WrongNonce,
    AlgNone,
    /// Every endpoint answers 503.
    Unavailable,
    /// Every endpoint sleeps 60 s before answering.
    Hang,
    /// Every endpoint answers 302 to itself.
    Redirect,
    /// Every endpoint answers 2 MiB of JSON-looking padding.
    Oversized,
}

/// The user the mock signs in.
#[derive(Clone, Debug)]
pub struct Identity {
    pub subject: String,
    pub email: Option<String>,
    pub preferred_username: Option<String>,
    pub name: Option<String>,
}

impl Identity {
    pub fn new(subject: &str) -> Identity {
        Identity {
            subject: subject.into(),
            email: None,
            preferred_username: None,
            name: None,
        }
    }
}

struct Grant {
    nonce: String,
    challenge: String,
    redirect_uri: String,
}

struct Inner {
    issuer: String,
    fault: Fault,
    key: usize,
    identity: Identity,
    grants: HashMap<String, Grant>,
    next_code: u64,
    discovery_calls: u32,
    jwks_calls: u32,
    token_calls: u32,
}

#[derive(Clone)]
struct Shared(Arc<Mutex<Inner>>);

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub struct MockIdp {
    addr: SocketAddr,
    shared: Shared,
    _task: tokio::task::JoinHandle<()>,
}

impl MockIdp {
    pub async fn start() -> MockIdp {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Shared(Arc::new(Mutex::new(Inner {
            issuer: format!("http://{addr}"),
            fault: Fault::None,
            key: 0,
            identity: Identity::new("subject-1"),
            grants: HashMap::new(),
            next_code: 1,
            discovery_calls: 0,
            jwks_calls: 0,
            token_calls: 0,
        })));
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/authorize", get(authorize))
            .route("/token", post(token))
            .with_state(shared.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        MockIdp {
            addr,
            shared,
            _task: task,
        }
    }

    /// `http://127.0.0.1:<port>`, no trailing slash.
    pub fn issuer(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn set_fault(&self, fault: Fault) {
        self.shared.lock().fault = fault;
    }

    pub fn set_identity(&self, identity: Identity) {
        self.shared.lock().identity = identity;
    }

    /// Signs with the other fixture key from now on and publishes only that key.
    pub fn rotate_key(&self) {
        let mut inner = self.shared.lock();
        inner.key = 1 - inner.key;
    }

    pub fn discovery_calls(&self) -> u32 {
        self.shared.lock().discovery_calls
    }

    pub fn jwks_calls(&self) -> u32 {
        self.shared.lock().jwks_calls
    }

    pub fn token_calls(&self) -> u32 {
        self.shared.lock().token_calls
    }

    /// Settings for this IdP with the client secret written to `dir`.
    pub fn settings(&self, dir: &Path, jit: bool) -> OidcSettings {
        let file = dir.join("oidc-client-secret");
        std::fs::write(&file, format!("{CLIENT_SECRET}\n")).unwrap();
        OidcSettings::from_flags(
            Some(&self.issuer()),
            Some(CLIENT_ID),
            Some(&file),
            None,
            jit,
            false,
        )
        .unwrap()
        .unwrap()
    }

    /// Plays the browser at the IdP: GETs `authorize_url` without following the redirect and
    /// returns the `code` and `state` the IdP sent back to the redirect URI.
    pub async fn authorize(&self, authorize_url: &str) -> (String, String) {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let response = client.get(authorize_url).send().await.unwrap();
        assert_eq!(response.status(), 302, "authorize did not redirect");
        let location = response.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .to_owned();
        let url = url::Url::parse(&location).unwrap();
        let param = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
                .unwrap()
        };
        (param("code"), param("state"))
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn signing_key(index: usize) -> CoreRsaPrivateSigningKey {
    let (kid, pem) = KEYS[index];
    CoreRsaPrivateSigningKey::from_pem(pem, Some(JsonWebKeyId::new(kid.into()))).unwrap()
}

// The transport faults answer every endpoint the same way; `None` means answer normally.
async fn transport_fault(fault: Fault, uri: &Uri) -> Option<Response> {
    match fault {
        Fault::Unavailable => Some(StatusCode::SERVICE_UNAVAILABLE.into_response()),
        Fault::Hang => {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Some(StatusCode::SERVICE_UNAVAILABLE.into_response())
        }
        Fault::Redirect => {
            Some((StatusCode::FOUND, [(header::LOCATION, uri.to_string())]).into_response())
        }
        Fault::Oversized => {
            Some(format!("{{\"pad\":\"{}\"}}", "x".repeat(2 * 1024 * 1024)).into_response())
        }
        _ => None,
    }
}

fn json_response(value: Value) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
        .into_response()
}

async fn discovery(State(shared): State<Shared>, uri: Uri) -> Response {
    let (fault, issuer) = {
        let mut inner = shared.lock();
        inner.discovery_calls += 1;
        (inner.fault, inner.issuer.clone())
    };
    if let Some(answer) = transport_fault(fault, &uri).await {
        return answer;
    }
    json_response(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
    }))
}

async fn jwks(State(shared): State<Shared>, uri: Uri) -> Response {
    let (fault, key) = {
        let mut inner = shared.lock();
        inner.jwks_calls += 1;
        (inner.fault, inner.key)
    };
    if let Some(answer) = transport_fault(fault, &uri).await {
        return answer;
    }
    json_response(json!({ "keys": [signing_key(key).as_verification_key()] }))
}

fn query(uri: &Uri) -> HashMap<String, String> {
    url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
        .into_owned()
        .collect()
}

async fn authorize(State(shared): State<Shared>, uri: Uri) -> Response {
    let q = query(&uri);
    let field = |name: &str| q.get(name).cloned().unwrap_or_default();
    if field("client_id") != CLIENT_ID
        || field("response_type") != "code"
        || field("code_challenge_method") != "S256"
    {
        return (StatusCode::BAD_REQUEST, "bad authorize request").into_response();
    }
    let mut inner = shared.lock();
    let code = format!("idp-code-{}", inner.next_code);
    inner.next_code += 1;
    let redirect_uri = field("redirect_uri");
    inner.grants.insert(
        code.clone(),
        Grant {
            nonce: field("nonce"),
            challenge: field("code_challenge"),
            redirect_uri: redirect_uri.clone(),
        },
    );
    let mut target = url::Url::parse(&redirect_uri).unwrap();
    target
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &field("state"));
    (StatusCode::FOUND, [(header::LOCATION, target.to_string())]).into_response()
}

fn token_error() -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"error":"invalid_grant"}"#,
    )
        .into_response()
}

async fn token(
    State(shared): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let fault = {
        let mut inner = shared.lock();
        inner.token_calls += 1;
        inner.fault
    };
    if let Some(answer) = transport_fault(fault, &uri).await {
        return answer;
    }
    let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    let basic = format!(
        "Basic {}",
        STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"))
    );
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        != Some(basic.as_str())
        || form.get("grant_type").map(String::as_str) != Some("authorization_code")
    {
        return token_error();
    }
    let mut inner = shared.lock();
    let Some(grant) = form.get("code").and_then(|code| inner.grants.remove(code)) else {
        return token_error();
    };
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    if URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) != grant.challenge
        || form.get("redirect_uri") != Some(&grant.redirect_uri)
    {
        return token_error();
    }
    let id_token = id_token(&inner, &grant.nonce);
    json_response(json!({
        "access_token": "idp-access-token",
        "token_type": "Bearer",
        "expires_in": 300,
        "id_token": id_token,
    }))
}

// Builds and signs the JWT by hand, so each claim fault is one field and `alg: none` is possible.
fn id_token(inner: &Inner, nonce: &str) -> String {
    let now = now_secs();
    let fault = inner.fault;
    let id = &inner.identity;
    let claims = json!({
        "iss": if fault == Fault::WrongIssuer { "https://other.example".to_owned() } else { inner.issuer.clone() },
        "aud": [if fault == Fault::WrongAudience { "someone-else" } else { CLIENT_ID }],
        "exp": if fault == Fault::Expired { now - 600 } else { now + 300 },
        "iat": if fault == Fault::Expired { now - 900 } else { now },
        "nonce": if fault == Fault::WrongNonce { "not-the-nonce" } else { nonce },
        "sub": id.subject,
        "email": id.email,
        "preferred_username": id.preferred_username,
        "name": id.name,
    });
    let (kid, _) = KEYS[inner.key];
    let header = if fault == Fault::AlgNone {
        json!({"alg": "none", "typ": "JWT"})
    } else {
        json!({"alg": "RS256", "typ": "JWT", "kid": kid})
    };
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    if fault == Fault::AlgNone {
        return format!("{input}.");
    }
    let signature = signing_key(inner.key)
        .sign(
            &CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
            input.as_bytes(),
        )
        .unwrap();
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
}
