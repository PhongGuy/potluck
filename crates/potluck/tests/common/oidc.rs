//! A server with OIDC pointed at the mock IdP, and a browser stand-in that never follows
//! redirects so each hop's `Location` can be checked.

// Shared test support: each test file uses a different subset of it (`dir` is read by plan C).
#![allow(dead_code)]

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use potluck::api::AppState;
use potluck::db::Db;
use potluck::oidc::{IdpHttp, OidcState, Provider, Timings};
use potluck::server::{Running, start_with};
use reqwest::header::{HeaderMap, LOCATION};
use sha2::{Digest, Sha256};
use url::Url;

use super::idp::MockIdp;
use super::{config_for, fast_passwords, migrated_db};

pub const VERIFIER: &str = "app-verifier-0123456789abcdefghijklmnopqrstuvwxyz";
pub const APP_STATE: &str = "app-state-0123456789";

pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn param(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

pub struct OidcApp {
    pub idp: MockIdp,
    pub dir: tempfile::TempDir,
    pub db: Db,
    pub running: Running,
}

pub struct Hop {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: String,
}

impl Hop {
    pub fn location(&self) -> Url {
        Url::parse(self.headers[LOCATION].to_str().unwrap()).unwrap()
    }
}

impl OidcApp {
    /// Start and redeem budgets of 1000 a minute, so tests that are not about limits never hit them.
    pub async fn start(jit: bool, timings: Timings) -> OidcApp {
        Self::start_with_limits(jit, timings, 1000).await
    }

    /// The production budget is 10.
    pub async fn start_with_limits(jit: bool, timings: Timings, per_min: u32) -> OidcApp {
        let idp = MockIdp::start().await;
        let (dir, db) = migrated_db().await;
        let config = config_for(dir.path());
        let http = IdpHttp::with_limits(
            false,
            Duration::from_secs(1),
            Duration::from_secs(2),
            1024 * 1024,
        )
        .unwrap();
        let provider = Provider::new(idp.settings(dir.path(), jit), http);
        let state = AppState::new(db.clone(), config.clone())
            .with_passwords(fast_passwords())
            .with_oidc(OidcState::new(provider, timings).with_rate_limits(per_min, per_min));
        let running = start_with(&config, state).await.expect("start");
        OidcApp {
            idp,
            dir,
            db,
            running,
        }
    }

    pub fn api(&self, path: &str) -> String {
        format!("http://{}/api/v1{path}", self.running.http_addr())
    }

    pub fn start_url(&self, port: u16, state: &str, challenge: &str) -> String {
        self.api(&format!(
            "/oidc/start?port={port}&state={state}&challenge={challenge}&challenge_method=S256"
        ))
    }

    pub async fn get(&self, url: &str) -> Hop {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let response = client.get(url).send().await.unwrap();
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.text().await.unwrap();
        Hop {
            status,
            headers,
            body,
        }
    }

    /// Start, IdP, callback: returns the callback's answer (normally 302 to the app's loopback).
    pub async fn browse(&self, start_url: &str) -> Hop {
        let start = self.get(start_url).await;
        assert_eq!(start.status, 302, "start: {}", start.body);
        let (code, state) = self.idp.authorize(start.location().as_str()).await;
        self.callback(&format!("code={code}&state={state}")).await
    }

    /// GET this server's callback with `query`, as the IdP's redirect would.
    pub async fn callback(&self, query: &str) -> Hop {
        self.get(&self.api(&format!("/oidc/callback?{query}")))
            .await
    }

    /// The full browser leg with the default app values: the loopback URL the app receives.
    pub async fn sign_in_to_loopback(&self) -> Url {
        let hop = self
            .browse(&self.start_url(49152, APP_STATE, &challenge(VERIFIER)))
            .await;
        assert_eq!(hop.status, 302, "callback: {}", hop.body);
        hop.location()
    }
}

pub fn assert_page(hop: &Hop, status: u16) {
    assert_eq!(hop.status, status, "{}", hop.body);
    assert!(
        hop.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert_eq!(hop.headers["content-security-policy"], "default-src 'none'");
    assert_eq!(hop.headers["cache-control"], "no-store");
    assert_eq!(hop.headers["referrer-policy"], "no-referrer");
    assert!(!hop.headers.contains_key(LOCATION));
}
