//! The update check in a started server: the admin socket, single-flight over HTTP, the daily
//! task at shutdown, and the flag and env var through the binary.

use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use potluck::admin::client::{AdminClient, Verb};
use potluck::server::{start, start_with_updates};
use reqwest::Method;
use serde_json::Value;

use crate::common::release::{ReleaseServer, Reply, TestKey, manifest};
use crate::common::{api_admin, call, config_for, potluck, unix_get};

async fn update_json(socket: &Path) -> Value {
    let (status, body) = unix_get(socket, "/api/v1/update").await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn system_over_the_socket_reads_and_runs_the_check() {
    let (server, key) = (ReleaseServer::start().await, TestKey::new());
    server.publish(&key, &manifest("9.0.0"));
    let dir = tempfile::tempdir().unwrap();
    let running = start_with_updates(&config_for(dir.path()), server.source(&key))
        .await
        .unwrap();
    let body = update_json(running.admin_socket()).await;
    assert_eq!(body["check"], "daily");
    assert_eq!(body["latest"], Value::Null);
    let response = AdminClient::connect(running.admin_socket())
        .await
        .unwrap()
        .send(Verb::Post, "/api/v1/update/check", None)
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text());
    let body: Value = serde_json::from_str(response.text()).unwrap();
    assert_eq!(body["latest"]["version"], "9.0.0");
    running.shutdown().await;
}

#[tokio::test]
async fn concurrent_posts_fetch_once() {
    let (server, key) = (ReleaseServer::start().await, TestKey::new());
    let body = manifest("9.0.0");
    server.set("potluck-release.json.sig", Reply::Body(key.sign(&body)));
    server.set(
        "potluck-release.json",
        Reply::Slow(Duration::from_millis(500), body),
    );
    let dir = tempfile::tempdir().unwrap();
    let running = start_with_updates(&config_for(dir.path()), server.source(&key))
        .await
        .unwrap();
    let admin = api_admin(running.db()).await;
    let url = format!("http://{}/api/v1/update/check", running.http_addr());
    let (a, b) = tokio::join!(
        call(&url, Method::POST, Some(&admin), None),
        call(&url, Method::POST, Some(&admin), None)
    );
    assert_eq!((a.0, b.0), (200, 200), "{} {}", a.1, b.1);
    assert_eq!(a.1, b.1);
    assert_eq!(server.manifest_hits(), 1);
    running.shutdown().await;
}

#[tokio::test]
async fn shutdown_returns_promptly_while_the_first_check_waits() {
    let (server, key) = (ReleaseServer::start().await, TestKey::new());
    server.publish(&key, &manifest("9.0.0"));
    let dir = tempfile::tempdir().unwrap();
    let running = start_with_updates(&config_for(dir.path()), server.source(&key))
        .await
        .unwrap();
    let began = Instant::now();
    running.shutdown().await;
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(
        server.manifest_hits(),
        0,
        "the first check waits five minutes"
    );
}

#[tokio::test]
async fn a_plain_start_reports_the_check_off() {
    let dir = tempfile::tempdir().unwrap();
    let running = start(&config_for(dir.path())).await.unwrap();
    let body = update_json(running.admin_socket()).await;
    assert_eq!(body["check"], "off");
    assert_eq!(body["latest"], Value::Null);
    running.shutdown().await;
}

/// A `potluck serve` child, killed on drop.
struct Serve(Child);

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `potluck serve` on `data` with `flags` and `env`. Its source is the real GitHub one, but the
/// first check waits five minutes and every child here is killed within seconds.
fn spawn_serve(data: &Path, flags: &[&str], env: &[(&str, &str)]) -> Serve {
    let mut cmd = potluck();
    cmd.args([
        "serve",
        "--listen",
        "127.0.0.1:0",
        "--public-url",
        "http://127.0.0.1:8080",
        "--data",
    ])
    .arg(data)
    .args(flags)
    .envs(env.iter().copied())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    let child = Serve(cmd.spawn().expect("spawn potluck serve"));
    let socket = data.join("admin.sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(
            Instant::now() < deadline,
            "admin.sock did not appear within 10 s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    child
}

/// Serve flags, environment variables, and the `check` value they should produce.
type Case<'a> = (&'a [&'a str], &'a [(&'a str, &'a str)], &'a str);

#[tokio::test]
async fn the_flag_and_the_env_var_turn_the_daily_check_off() {
    let cases: [Case; 4] = [
        (&[], &[], "daily"),
        (&["--no-update-check"], &[], "off"),
        (&[], &[("POTLUCK_UPDATE_CHECK", "false")], "off"),
        (&[], &[("POTLUCK_UPDATE_CHECK", "true")], "daily"),
    ];
    for (flags, env, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let _serve = spawn_serve(dir.path(), flags, env);
        let body = update_json(&dir.path().join("admin.sock")).await;
        assert_eq!(body["check"], expected, "{flags:?} {env:?}");
    }
}

#[test]
fn a_non_boolean_env_value_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = potluck()
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--public-url",
            "http://127.0.0.1:8080",
            "--data",
        ])
        .arg(dir.path())
        .env("POTLUCK_UPDATE_CHECK", "0")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "clap refuses it before anything starts"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("--no-update-check"));
}
