//! Release fixtures for the update checker: a minisign key made at test time and what it signs.

use std::collections::HashMap;
use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use potluck::update::{TrustedKeys, UpdateSource};

/// A well-formed digest for test manifests.
pub const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A well-formed manifest for `version`, as `release.sh manifest` writes it.
pub fn manifest(version: &str) -> Vec<u8> {
    serde_json::json!({
        "version": version,
        "image": "ghcr.io/phongguy/potluck",
        "digest": DIGEST,
        "published": "2026-11-20T10:00:00Z",
        "notes_url": format!("https://github.com/PhongGuy/potluck/releases/tag/v{version}"),
    })
    .to_string()
    .into_bytes()
}

/// A fresh unencrypted minisign key pair.
pub struct TestKey {
    pair: minisign::KeyPair,
    line: String,
}

impl TestKey {
    pub fn new() -> TestKey {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().expect("keypair");
        let text = pair.pk.to_box().expect("public key box").into_string();
        TestKey {
            line: STANDARD.encode(text),
            pair,
        }
    }

    /// The key as one line of `update-keys.txt`: the `.pub` file's text, base64.
    pub fn keys_line(&self) -> &str {
        &self.line
    }

    /// The key id as minisign prints it: the key number little-endian, uppercase hex.
    pub fn key_id(&self) -> String {
        self.pair
            .pk
            .keynum()
            .iter()
            .rev()
            .map(|b| format!("{b:02X}"))
            .collect()
    }

    /// A key list holding only this key.
    pub fn trusted(&self) -> TrustedKeys {
        TrustedKeys::parse(&self.line).expect("test key parses")
    }

    /// The `.sig` file for `bytes`: the prehashed minisign signature box, base64 on one line.
    pub fn sign(&self, bytes: &[u8]) -> Vec<u8> {
        let sig = minisign::sign(None, &self.pair.sk, Cursor::new(bytes.to_vec()), None, None)
            .expect("sign");
        format!("{}\n", STANDARD.encode(sig.into_string())).into_bytes()
    }
}

/// What the release server answers for one file.
#[derive(Clone)]
pub enum Reply {
    Body(Vec<u8>),
    Status(u16),
    Redirect(String),
    Slow(Duration, Vec<u8>),
}

#[derive(Default)]
struct Served {
    replies: HashMap<String, Reply>,
    manifest_hits: usize,
    user_agent: Option<String>,
    query: Option<String>,
}

/// A loopback server answering `/dl/{file}`; every file is 404 until set.
pub struct ReleaseServer {
    pub addr: SocketAddr,
    served: Arc<Mutex<Served>>,
    _task: tokio::task::JoinHandle<()>,
}

impl ReleaseServer {
    pub async fn start() -> ReleaseServer {
        let served = Arc::new(Mutex::new(Served::default()));
        let app = axum::Router::new()
            .route("/dl/{file}", axum::routing::get(reply))
            .with_state(Arc::clone(&served));
        let (addr, task) = crate::common::serve(app).await;
        ReleaseServer {
            addr,
            served,
            _task: task,
        }
    }

    pub fn base(&self) -> String {
        format!("http://{}/dl/", self.addr)
    }

    pub fn set(&self, file: &str, reply: Reply) {
        self.served
            .lock()
            .unwrap()
            .replies
            .insert(file.to_owned(), reply);
    }

    /// Serves `manifest` and its signature by `key`.
    pub fn publish(&self, key: &TestKey, manifest: &[u8]) {
        self.set("potluck-release.json", Reply::Body(manifest.to_vec()));
        self.set("potluck-release.json.sig", Reply::Body(key.sign(manifest)));
    }

    /// How many times the manifest was requested.
    pub fn manifest_hits(&self) -> usize {
        self.served.lock().unwrap().manifest_hits
    }

    /// The `User-Agent` of the last manifest request.
    pub fn user_agent(&self) -> Option<String> {
        self.served.lock().unwrap().user_agent.clone()
    }

    /// The query string of the last manifest request.
    pub fn query(&self) -> Option<String> {
        self.served.lock().unwrap().query.clone()
    }

    /// A source on this server trusting `key`, with test timeouts.
    pub fn source(&self, key: &TestKey) -> UpdateSource {
        UpdateSource::new(&self.base(), key.trusted())
            .expect("loopback source")
            .with_timeouts(Duration::from_secs(2), Duration::from_secs(5))
    }
}

async fn reply(
    State(served): State<Arc<Mutex<Served>>>,
    Path(file): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let reply = {
        let mut served = served.lock().unwrap();
        if file == "potluck-release.json" {
            served.manifest_hits += 1;
            served.user_agent = headers
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            served.query = uri.query().map(str::to_owned);
        }
        served.replies.get(&file).cloned()
    };
    match reply {
        None => StatusCode::NOT_FOUND.into_response(),
        Some(Reply::Body(body)) => body.into_response(),
        Some(Reply::Status(code)) => StatusCode::from_u16(code).expect("status").into_response(),
        Some(Reply::Redirect(to)) => (StatusCode::FOUND, [(header::LOCATION, to)]).into_response(),
        Some(Reply::Slow(delay, body)) => {
            tokio::time::sleep(delay).await;
            body.into_response()
        }
    }
}
