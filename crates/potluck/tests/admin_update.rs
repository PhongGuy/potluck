//! `potluck admin update check` against a server with a loopback update source.

use potluck::server::start_with_updates;
use serde_json::Value;

use crate::common::release::{DIGEST, ReleaseServer, TestKey, manifest};
use crate::common::{admin, config_for, potluck};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A started server whose source serves `version` signed by `signer`, trusting `key`.
async fn server_with(
    version: &str,
    key: &TestKey,
    signer: &TestKey,
) -> (ReleaseServer, tempfile::TempDir, potluck::server::Running) {
    let server = ReleaseServer::start().await;
    server.publish(signer, &manifest(version));
    let dir = tempfile::tempdir().unwrap();
    let running = start_with_updates(&config_for(dir.path()), server.source(key))
        .await
        .unwrap();
    (server, dir, running)
}

#[tokio::test]
async fn an_available_release_prints_digest_pinned_steps() {
    let key = TestKey::new();
    let (_server, dir, running) = server_with("9.0.0", &key, &key).await;
    let (result, out) = admin(dir.path(), &["update", "check"], "").await;
    result.unwrap();
    let pinned = format!("ghcr.io/phongguy/potluck@{DIGEST}");
    assert!(
        out.starts_with(&format!("potluck 9.0.0 is available (running {VERSION})\n")),
        "{out}"
    );
    for needle in [
        "published: 2026-11-20T10:00:00Z".to_owned(),
        "notes:     https://github.com/PhongGuy/potluck/releases/tag/v9.0.0".to_owned(),
        format!("image:     {pinned}"),
        format!("podman pull {pinned}"),
        "podman auto-update".to_owned(),
        format!("Image={pinned}"),
        format!("image: {pinned}"),
        "docker compose up -d".to_owned(),
        format!("gh attestation verify oci://{pinned} --repo PhongGuy/potluck"),
    ] {
        assert!(out.contains(&needle), "missing {needle:?} in\n{out}");
    }
    for line in out.lines().filter(|l| l.contains(" pull ")) {
        assert!(
            line.contains(&pinned),
            "every pull names the digest: {line}"
        );
    }
    running.shutdown().await;
}

#[tokio::test]
async fn an_up_to_date_server_prints_its_version_and_when_it_checked() {
    let key = TestKey::new();
    let (_server, dir, running) = server_with(VERSION, &key, &key).await;
    let (result, out) = admin(dir.path(), &["update", "check"], "").await;
    result.unwrap();
    assert!(
        out.starts_with(&format!("potluck {VERSION} is up to date (checked ")),
        "{out}"
    );
    assert!(out.ends_with("Z)\n"), "{out}");
    assert_eq!(out.lines().count(), 1, "{out}");
    running.shutdown().await;
}

#[tokio::test]
async fn json_prints_the_update_object() {
    let key = TestKey::new();
    let (_server, dir, running) = server_with("9.0.0", &key, &key).await;
    let (result, out) = admin(dir.path(), &["--json", "update", "check"], "").await;
    result.unwrap();
    let body: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(body["latest"]["digest"], DIGEST);
    assert_eq!(body["check"], "daily");
    running.shutdown().await;
}

#[tokio::test]
async fn a_failed_check_is_an_error_naming_the_class() {
    let (key, stranger) = (TestKey::new(), TestKey::new());
    let (_server, dir, running) = server_with("9.0.0", &key, &stranger).await;
    let (result, out) = admin(dir.path(), &["update", "check"], "").await;
    assert_eq!(
        result.unwrap_err().to_string(),
        "unavailable: signature by an unknown key"
    );
    assert_eq!(out, "");
    running.shutdown().await;
}

#[test]
fn admin_help_lists_the_update_noun() {
    let out = potluck()
        .args(["admin", "update", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("check"));
}
