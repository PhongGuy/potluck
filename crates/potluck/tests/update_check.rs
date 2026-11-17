//! The update checker against a loopback release server: what a check offers, its state, every
//! failure class, the fetch limits and the log lines (the last two parts arrive with later tasks).

use std::time::{Duration, Instant};

use potluck::update::{UpdateChecker, UpdateError, UpdateSource};

use crate::common::release::{ReleaseServer, Reply, TestKey, manifest};

const MANIFEST: &str = "potluck-release.json";
const SIG: &str = "potluck-release.json.sig";

fn checker(server: &ReleaseServer, key: &TestKey) -> UpdateChecker {
    UpdateChecker::new(server.source(key), "1.0.0").unwrap()
}

/// A check on a fresh server prepared by `setup`; it must fail.
async fn failure(setup: impl FnOnce(&ReleaseServer, &TestKey)) -> UpdateError {
    let (server, key) = (ReleaseServer::start().await, TestKey::new());
    setup(&server, &key);
    checker(&server, &key).check().await.unwrap_err()
}

#[tokio::test]
async fn a_newer_final_release_is_offered_and_kept_in_the_status() {
    let (server, key) = (ReleaseServer::start().await, TestKey::new());
    server.publish(&key, &manifest("1.1.0"));
    let c = checker(&server, &key);
    assert_eq!(c.status().latest(), None);
    assert_eq!(c.status().checked_at(), None);
    let status = c.check().await.unwrap();
    assert_eq!(status.current(), "1.0.0");
    assert_eq!(status.latest().map(|r| r.version()), Some("1.1.0"));
    assert!(status.checked_at().is_some_and(|t| t.ends_with('Z')));
    assert_eq!(c.status(), status);
}

#[tokio::test]
async fn the_fetch_sends_the_version_as_user_agent_and_no_query() {
    let (server, key) = (ReleaseServer::start().await, TestKey::new());
    server.publish(&key, &manifest("1.1.0"));
    checker(&server, &key).check().await.unwrap();
    assert_eq!(server.manifest_hits(), 1);
    assert_eq!(server.user_agent().as_deref(), Some("potluck/1.0.0"));
    assert_eq!(server.query(), None);
}

#[tokio::test]
async fn a_missing_file_is_not_found() {
    assert_eq!(failure(|_, _| {}).await, UpdateError::NotFound);
    let no_sig = failure(|s, _| s.set(MANIFEST, Reply::Body(manifest("1.1.0")))).await;
    assert_eq!(no_sig, UpdateError::NotFound);
}

#[tokio::test]
async fn http_level_failures_are_classed() {
    assert_eq!(
        failure(|s, _| s.set(MANIFEST, Reply::Status(500))).await,
        UpdateError::Status { status: 500 }
    );
    let to_http = |s: &ReleaseServer, _: &TestKey| {
        s.set(MANIFEST, Reply::Redirect("http://127.0.0.1:1/x".into()))
    };
    assert_eq!(failure(to_http).await, UpdateError::Redirect);
    let big =
        |s: &ReleaseServer, _: &TestKey| s.set(MANIFEST, Reply::Body(vec![b' '; 64 * 1024 + 1]));
    assert_eq!(failure(big).await, UpdateError::TooLarge);
}

#[tokio::test]
async fn signature_and_manifest_failures_are_classed() {
    let not_a_sig = |s: &ReleaseServer, _: &TestKey| {
        s.set(MANIFEST, Reply::Body(manifest("1.1.0")));
        s.set(SIG, Reply::Body(b"%%%".to_vec()));
    };
    assert_eq!(failure(not_a_sig).await, UpdateError::SignatureFormat);
    let other_key = |s: &ReleaseServer, _: &TestKey| {
        s.publish(&TestKey::new(), &manifest("1.1.0"));
    };
    assert_eq!(failure(other_key).await, UpdateError::UnknownKey);
    let wrong_bytes = |s: &ReleaseServer, k: &TestKey| {
        s.set(MANIFEST, Reply::Body(manifest("1.1.0")));
        s.set(SIG, Reply::Body(k.sign(&manifest("1.2.0"))));
    };
    assert_eq!(failure(wrong_bytes).await, UpdateError::BadSignature);
    let bad_digest = |s: &ReleaseServer, k: &TestKey| {
        let mut doc: serde_json::Value = serde_json::from_slice(&manifest("1.1.0")).unwrap();
        doc["digest"] = serde_json::json!("sha256:abc");
        s.publish(k, doc.to_string().as_bytes());
    };
    assert_eq!(
        failure(bad_digest).await,
        UpdateError::Manifest { field: "digest" }
    );
}

#[tokio::test]
async fn a_refused_connection_is_a_network_failure() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let key = TestKey::new();
    let source = UpdateSource::new(&format!("http://127.0.0.1:{port}/"), key.trusted()).unwrap();
    let err = UpdateChecker::new(source, "1.0.0")
        .unwrap()
        .check()
        .await
        .unwrap_err();
    assert_eq!(err, UpdateError::Network);
}

#[tokio::test]
async fn a_stalled_server_hits_the_total_timeout() {
    let (server, key) = (ReleaseServer::start().await, TestKey::new());
    server.set(
        MANIFEST,
        Reply::Slow(Duration::from_secs(30), manifest("1.1.0")),
    );
    let source = server
        .source(&key)
        .with_timeouts(Duration::from_secs(1), Duration::from_secs(1));
    let began = Instant::now();
    let err = UpdateChecker::new(source, "1.0.0")
        .unwrap()
        .check()
        .await
        .unwrap_err();
    assert_eq!(err, UpdateError::Network);
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "{:?}",
        began.elapsed()
    );
}

#[tokio::test]
async fn a_current_version_that_is_not_semver_is_refused() {
    let (server, key) = (ReleaseServer::start().await, TestKey::new());
    assert!(matches!(
        UpdateChecker::new(server.source(&key), "dev"),
        Err(UpdateError::CurrentVersion { .. })
    ));
}
