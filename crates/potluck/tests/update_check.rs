//! The update checker against a loopback release server: what a check offers, its state, every
//! failure class, the fetch limits and the log lines (the last two parts arrive with later tasks).

use std::time::{Duration, Instant};

use potluck::update::{UpdateChecker, UpdateError, UpdateSource};

use crate::common::release::{ReleaseServer, Reply, TestKey, manifest};
use crate::logs::captured_logs;

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

#[tokio::test]
async fn an_equal_older_or_pre_release_version_is_up_to_date() {
    for version in ["1.0.0", "0.9.9", "1.1.0-rc.1", "1.0.0+build.7"] {
        let (server, key) = (ReleaseServer::start().await, TestKey::new());
        server.publish(&key, &manifest(version));
        let status = checker(&server, &key).check().await.unwrap();
        assert_eq!(status.latest(), None, "{version}");
        assert!(
            status.checked_at().is_some(),
            "{version} is a verified answer"
        );
    }
}

#[tokio::test]
async fn latest_only_moves_up() {
    let (server, key) = (ReleaseServer::start().await, TestKey::new());
    let c = checker(&server, &key);
    server.publish(&key, &manifest("1.3.0"));
    c.check().await.unwrap();
    server.publish(&key, &manifest("1.2.0"));
    let status = c.check().await.unwrap();
    assert_eq!(
        status.latest().map(|r| r.version()),
        Some("1.3.0"),
        "a replayed older manifest cannot push latest back"
    );
    server.publish(&key, &manifest("1.4.0"));
    let status = c.check().await.unwrap();
    assert_eq!(status.latest().map(|r| r.version()), Some("1.4.0"));
}

#[tokio::test]
async fn a_failed_check_changes_neither_field() {
    let (server, key) = (ReleaseServer::start().await, TestKey::new());
    let c = checker(&server, &key);
    server.publish(&key, &manifest("1.3.0"));
    c.check().await.unwrap();
    let before = c.status();
    server.set(SIG, Reply::Body(TestKey::new().sign(&manifest("1.3.0"))));
    assert_eq!(c.check().await, Err(UpdateError::UnknownKey));
    assert_eq!(c.status(), before);
}

#[test]
fn a_new_release_is_logged_once_at_info_and_failures_at_warn_or_debug() {
    let text = captured_logs(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (server, key) = (ReleaseServer::start().await, TestKey::new());
            let c = checker(&server, &key);
            let _ = c.check().await;
            server.publish(&key, &manifest("1.1.0"));
            c.check().await.unwrap();
            c.check().await.unwrap();
            server.set(SIG, Reply::Body(b"%%%".to_vec()));
            let _ = c.check().await;
        });
    });
    let offered: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("potluck 1.1.0 is available"))
        .collect();
    assert_eq!(offered.len(), 1, "{text}");
    for needle in [
        " INFO ",
        "running 1.0.0",
        "https://github.com/PhongGuy/potluck/releases/tag/v1.1.0",
        "potluck admin update check",
    ] {
        assert!(offered[0].contains(needle), "{needle:?} in {}", offered[0]);
    }
    let failed: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("update check failed"))
        .collect();
    assert_eq!(failed.len(), 1, "only the bad signature warns: {text}");
    for needle in [" WARN ", "not a minisign signature", "--no-update-check"] {
        assert!(failed[0].contains(needle), "{needle:?} in {}", failed[0]);
    }
    assert!(
        text.lines()
            .any(|l| l.contains(" DEBUG ") && l.contains("no signed release is published")),
        "the 404 logs at debug: {text}"
    );
}

#[tokio::test]
async fn concurrent_checks_share_one_fetch() {
    let (server, key) = (ReleaseServer::start().await, TestKey::new());
    let body = manifest("1.1.0");
    server.set(SIG, Reply::Body(key.sign(&body)));
    server.set(MANIFEST, Reply::Slow(Duration::from_millis(300), body));
    let c = checker(&server, &key);
    let (a, b, d) = tokio::join!(c.check(), c.check(), c.check());
    assert_eq!(server.manifest_hits(), 1);
    let a = a.unwrap();
    assert_eq!(b.unwrap(), a);
    assert_eq!(d.unwrap(), a);
    c.check().await.unwrap();
    assert_eq!(
        server.manifest_hits(),
        2,
        "a check after the answer fetches again"
    );
}

#[tokio::test]
async fn a_caller_that_gives_up_does_not_cancel_the_shared_check() {
    let (server, key) = (ReleaseServer::start().await, TestKey::new());
    let body = manifest("1.1.0");
    server.set(SIG, Reply::Body(key.sign(&body)));
    server.set(MANIFEST, Reply::Slow(Duration::from_millis(300), body));
    let c = checker(&server, &key);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), c.check())
            .await
            .is_err()
    );
    let status = c.check().await.unwrap();
    assert_eq!(status.latest().map(|r| r.version()), Some("1.1.0"));
    assert_eq!(
        server.manifest_hits(),
        1,
        "the second caller joined the first fetch"
    );
}
