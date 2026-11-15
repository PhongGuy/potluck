//! `server::start` end to end on a temp data dir: TCP, the admin socket, the data layout,
//! shutdown, and the `serve` subcommand's refusals through the binary.

use potluck::server::{ServerError, start};
use serde_json::Value;

use crate::common::{config_for, config_listening_on, create_user, potluck, unix_get};

#[tokio::test]
async fn start_serves_health_on_loopback_and_prepares_the_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let running = start(&config_for(dir.path())).await.unwrap();
    assert_eq!(running.http_addr().ip().to_string(), "127.0.0.1");
    let body: Value = reqwest::get(format!("http://{}/api/v1/health", running.http_addr()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["status"], "ok");
    assert!(dir.path().join("server.db").is_file(), "server.db");
    assert!(dir.path().join("repos").is_dir(), "repos/");
    assert!(dir.path().join("hooks").is_dir(), "hooks/");
    let version: i64 = rusqlite::Connection::open(dir.path().join("server.db"))
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 1, "migrations ran at start");
    running.shutdown().await;
}

#[tokio::test]
async fn start_creates_a_missing_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("nested").join("data");
    let running = start(&config_for(&data)).await.unwrap();
    assert!(data.join("server.db").is_file());
    running.shutdown().await;
}

#[tokio::test]
async fn start_reports_an_unwritable_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("file");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let err = start(&config_for(&blocker.join("data")))
        .await
        .expect_err("a file cannot be a parent");
    assert!(matches!(err, ServerError::DataDir { .. }), "got {err:?}");
}

#[tokio::test]
async fn the_admin_socket_serves_the_api_and_shutdown_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("admin.sock"), b"stale").unwrap();
    let running = start(&config_for(dir.path())).await.unwrap();
    let socket = running.admin_socket().to_path_buf();
    let (status, body) = unix_get(&socket, "/api/v1/health").await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["status"],
        "ok"
    );
    let (status, body) = unix_get(&socket, "/api/v1/nope").await;
    assert_eq!(status, 404);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
        "not_found"
    );
    running.shutdown().await;
    assert!(!socket.exists(), "shutdown removes the socket file");
}

#[tokio::test]
async fn shutdown_closes_the_tcp_listener_and_a_restart_reuses_the_data() {
    let dir = tempfile::tempdir().unwrap();
    // A bound, never-listening SO_REUSEADDR socket reserves the port for this test: the kernel
    // hands it to no other bind(0) while the guard lives, yet our own listener may share it.
    let guard = tokio::net::TcpSocket::new_v4().unwrap();
    guard.set_reuseaddr(true).unwrap();
    guard.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = guard.local_addr().unwrap();
    let config = config_listening_on(dir.path(), addr);
    let running = start(&config).await.unwrap();
    assert_eq!(
        running.http_addr(),
        addr,
        "the server took the reserved port"
    );
    create_user(running.db(), "alice", "correct horse battery", false).await;
    running.shutdown().await;
    assert!(
        tokio::net::TcpStream::connect(addr).await.is_err(),
        "the port is closed"
    );
    let again = start(&config).await.unwrap();
    assert_eq!(again.http_addr(), addr, "the restart binds the same port");
    let body: Value = reqwest::get(format!("http://{addr}/api/v1/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["status"], "ok");
    let alice = again.db().user_by_username("alice").await.unwrap();
    assert!(alice.is_some(), "the restart serves the first run's data");
    again.shutdown().await;
    drop(guard);
}

#[test]
fn serve_without_public_url_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = potluck()
        .args(["serve", "--data"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--public-url"));
}

#[test]
fn serve_refuses_a_non_loopback_http_public_url_from_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let out = potluck()
        .env("POTLUCK_PUBLIC_URL", "http://team.example.com")
        .args(["serve", "--data"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--insecure-http"), "{stderr}");
    assert!(stderr.contains("team.example.com"), "{stderr}");
    assert!(
        !dir.path().join("server.db").exists(),
        "nothing is created before config passes"
    );
}

#[test]
fn serve_on_a_busy_port_prints_the_cause_once() {
    let dir = tempfile::tempdir().unwrap();
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = busy.local_addr().unwrap().to_string();
    let out = potluck()
        .args([
            "serve",
            "--listen",
            &addr,
            "--public-url",
            "http://127.0.0.1:8080",
            "--data",
        ])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "a bind failure exits 1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.matches("Address already in use").count(),
        1,
        "{stderr}"
    );
}

#[test]
fn serve_refuses_oidc_flags_from_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let out = potluck()
        .env("POTLUCK_PUBLIC_URL", "http://127.0.0.1:8080")
        .env("POTLUCK_OIDC_CLIENT_ID", "potluck")
        .args(["serve", "--data"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("OIDC"));
    assert!(
        !dir.path().join("server.db").exists(),
        "nothing is created before config passes"
    );
}
