//! The metrics listener: off by default, OpenMetrics on `/metrics`, nothing else.

use std::net::SocketAddr;

use potluck::admin::client::{AdminClient, Verb};
use potluck::api::AppState;
use potluck::db::Db;
use potluck::git::access::PushContext;
use potluck::role::Role;
use potluck::server::{Running, ServerError, start, start_with, start_with_updates};
use potluck::slug::Slug;

use crate::common::release::{ReleaseServer, TestKey, manifest};
use crate::common::{
    APINATA_FORMAT, TestAccess, client, config_for, create_user, failed, fast_passwords, git,
    git_server, ok, parse_config, work_tree,
};

/// The value of the first sample whose series is exactly `series`.
pub fn sample(text: &str, series: &str) -> Option<f64> {
    text.lines()
        .find_map(|l| {
            l.strip_prefix(series)
                .and_then(|rest| rest.strip_prefix(' '))
        })
        .and_then(|v| v.parse().ok())
}

/// A started server with `--metrics-listen 127.0.0.1:0` and fast hashing.
pub async fn metrics_server() -> (tempfile::TempDir, Db, Running) {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap().to_owned();
    let config = parse_config(&[
        "--listen",
        "127.0.0.1:0",
        "--metrics-listen",
        "127.0.0.1:0",
        "--data",
        &data,
        "--public-url",
        "http://127.0.0.1:8080",
    ])
    .unwrap();
    std::fs::create_dir_all(config.data()).unwrap();
    let db = Db::open(&config.db_path()).unwrap();
    let state = AppState::new(db.clone(), config.clone()).with_passwords(fast_passwords());
    let running = start_with(&config, state).await.unwrap();
    (dir, db, running)
}

/// `GET http://{addr}/metrics`, asserting 200 and the OpenMetrics content type.
pub async fn scrape(running: &Running) -> String {
    let addr = running.metrics_addr().expect("metrics listener");
    let r = client()
        .get(format!("http://{addr}/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        r.headers()["content-type"],
        "application/openmetrics-text; version=1.0.0; charset=utf-8"
    );
    r.text().await.unwrap()
}

#[tokio::test]
async fn without_the_flag_nothing_is_bound() {
    let dir = tempfile::tempdir().unwrap();
    let running = start(&config_for(dir.path())).await.unwrap();
    assert_eq!(running.metrics_addr(), None);
    running.shutdown().await;
}

#[tokio::test]
async fn the_listener_serves_openmetrics_on_its_own_port() {
    let (_dir, _db, running) = metrics_server().await;
    let addr = running.metrics_addr().unwrap();
    assert_ne!(addr, running.http_addr(), "a port of its own");
    assert!(addr.ip().is_loopback());
    let text = scrape(&running).await;
    let version = env!("CARGO_PKG_VERSION");
    assert!(
        text.contains(&format!("potluck_build_info{{version=\"{version}\"}} 1")),
        "{text}"
    );
    assert!(text.ends_with("# EOF\n"), "{text}");
    let build = format!("potluck_build_info{{version=\"{version}\"}}");
    assert_eq!(sample(&text, &build), Some(1.0), "{text}");
    running.shutdown().await;
}

#[tokio::test]
async fn other_paths_and_methods_on_the_metrics_port_are_empty_404s() {
    let (_dir, _db, running) = metrics_server().await;
    let addr = running.metrics_addr().unwrap();
    for (method, path) in [
        (reqwest::Method::GET, "/"),
        (reqwest::Method::GET, "/api/v1/health"),
        (reqwest::Method::POST, "/metrics"),
        (reqwest::Method::GET, "/metrics/x"),
    ] {
        let r = client()
            .request(method.clone(), format!("http://{addr}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 404, "{method} {path}");
        assert!(r.text().await.unwrap().is_empty(), "{method} {path}");
    }
    let api = client()
        .get(format!("http://{}/metrics", running.http_addr()))
        .send()
        .await
        .unwrap();
    assert_eq!(api.status().as_u16(), 404, "the API port serves no metrics");
    running.shutdown().await;
}

#[tokio::test]
async fn a_taken_metrics_port_stops_the_start_naming_that_address() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port: SocketAddr = taken.local_addr().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap().to_owned();
    let flag = port.to_string();
    let config = parse_config(&[
        "--listen",
        "127.0.0.1:0",
        "--metrics-listen",
        &flag,
        "--data",
        &data,
        "--public-url",
        "http://127.0.0.1:8080",
    ])
    .unwrap();
    match start(&config).await {
        Err(ServerError::Listen { addr, .. }) => assert_eq!(addr, port),
        other => panic!("expected Listen, got {other:?}"),
    }
}

#[tokio::test]
async fn shutdown_closes_the_metrics_port() {
    let (_dir, _db, running) = metrics_server().await;
    let addr = running.metrics_addr().unwrap();
    running.shutdown().await;
    assert!(
        tokio::net::TcpStream::connect(addr).await.is_err(),
        "nothing accepts after shutdown"
    );
}

#[tokio::test]
async fn the_real_router_labels_unknown_paths_unmatched_and_known_ones_by_template() {
    let (_dir, _db, running) = metrics_server().await;
    let base = format!("http://{}", running.http_addr());
    assert_eq!(
        client()
            .get(format!("{base}/api/v1/no-such-route"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        404
    );
    assert_eq!(
        client()
            .get(format!("{base}/api/v1/health"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );
    let text = scrape(&running).await;
    let unmatched = "http_server_request_duration_seconds_count{http_request_method=\"GET\",http_route=\"unmatched\",http_response_status_code=\"404\"}";
    let health = "http_server_request_duration_seconds_count{http_request_method=\"GET\",http_route=\"/api/v1/health\",http_response_status_code=\"200\"}";
    assert_eq!(sample(&text, unmatched), Some(1.0), "{text}");
    assert_eq!(sample(&text, health), Some(1.0), "{text}");
    assert!(!text.contains("no-such-route"), "{text}");
    running.shutdown().await;
}

async fn post_session(running: &Running, username: &str, password: &str) -> u16 {
    client()
        .post(format!("http://{}/api/v1/session", running.http_addr()))
        .json(&serde_json::json!({"username": username, "password": password, "token_name": "t"}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn sign_ins_count_success_failure_and_the_username_limit() {
    let (_dir, db, running) = metrics_server().await;
    create_user(&db, "alice", "right-password-1", false).await;
    assert_eq!(
        post_session(&running, "alice", "right-password-1").await,
        201
    );
    for _ in 0..4 {
        assert_eq!(
            post_session(&running, "alice", "wrong-password-9").await,
            401
        );
    }
    // The username window holds 5 attempts; the sixth is limited by name, not by address.
    assert_eq!(
        post_session(&running, "alice", "wrong-password-9").await,
        429
    );
    let text = scrape(&running).await;
    let series = |o: &str| format!("potluck_sign_ins_total{{method=\"password\",outcome=\"{o}\"}}");
    assert_eq!(sample(&text, &series("success")), Some(1.0), "{text}");
    assert_eq!(sample(&text, &series("failure")), Some(4.0), "{text}");
    assert_eq!(
        sample(&text, &series("rate_limited_username")),
        Some(1.0),
        "{text}"
    );
    for secret in ["alice", "right-password-1", "wrong-password-9"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    running.shutdown().await;
}

#[tokio::test]
async fn the_address_limit_is_counted_apart_from_the_username_limit() {
    let (_dir, _db, running) = metrics_server().await;
    // 10 attempts per address per window, each under a different name, then one more.
    for i in 0..10 {
        assert_eq!(
            post_session(&running, &format!("nobody{i}"), "x-password-1").await,
            401
        );
    }
    assert_eq!(
        post_session(&running, "nobody10", "x-password-1").await,
        429
    );
    let text = scrape(&running).await;
    let series = |o: &str| format!("potluck_sign_ins_total{{method=\"password\",outcome=\"{o}\"}}");
    assert_eq!(
        sample(&text, &series("rate_limited_ip")),
        Some(1.0),
        "{text}"
    );
    assert_eq!(sample(&text, &series("failure")), Some(10.0), "{text}");
    running.shutdown().await;
}

#[tokio::test]
async fn a_rejected_token_name_records_no_sign_in() {
    let (_dir, db, running) = metrics_server().await;
    create_user(&db, "alice", "right-password-1", false).await;
    let status = client()
        .post(format!("http://{}/api/v1/session", running.http_addr()))
        .json(&serde_json::json!({"username": "alice", "password": "right-password-1", "token_name": " "}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(status, 422);
    let text = scrape(&running).await;
    // The family may list zero-valued series, but no outcome may hold a sample.
    for line in text
        .lines()
        .filter(|l| l.starts_with("potluck_sign_ins_total{"))
    {
        let value = line.rsplit(' ').next().unwrap();
        assert_eq!(value.parse::<f64>().unwrap(), 0.0, "{text}");
    }
    running.shutdown().await;
}

#[tokio::test]
async fn a_bad_bearer_token_on_the_api_counts_one_auth_failure() {
    let (_dir, _db, running) = metrics_server().await;
    let r = client()
        .get(format!("http://{}/api/v1/me", running.http_addr()))
        .bearer_auth("plk_definitely_not_a_token")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    let text = scrape(&running).await;
    assert_eq!(
        sample(&text, "potluck_auth_failures_total{scheme=\"bearer\"}"),
        Some(1.0),
        "{text}"
    );
    assert!(!text.contains("plk_definitely"), "{text}");
    running.shutdown().await;
}

#[tokio::test]
async fn a_scrape_reads_slots_queue_and_the_update_check() {
    let (server, key) = (ReleaseServer::start().await, TestKey::new());
    server.publish(&key, &manifest("9.0.0"));
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap().to_owned();
    let config = parse_config(&[
        "--listen",
        "127.0.0.1:0",
        "--metrics-listen",
        "127.0.0.1:0",
        "--data",
        &data,
        "--public-url",
        "http://127.0.0.1:8080",
        "--no-update-check",
    ])
    .unwrap();
    let running = start_with_updates(&config, server.source(&key))
        .await
        .unwrap();
    // The audit pruner's start-up pass may still sit in the DB queue; an idle server drains it.
    let mut before = scrape(&running).await;
    for _ in 0..50 {
        if sample(&before, "potluck_db_queue_depth") == Some(0.0) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        before = scrape(&running).await;
    }
    assert_eq!(
        sample(&before, "potluck_update_available"),
        Some(0.0),
        "{before}"
    );
    assert_eq!(
        sample(&before, "potluck_git_slots_in_use"),
        Some(0.0),
        "{before}"
    );
    assert!(
        sample(&before, "potluck_git_slots_limit").unwrap() >= 2.0,
        "{before}"
    );
    assert_eq!(
        sample(&before, "potluck_sse_subscribers"),
        Some(0.0),
        "{before}"
    );
    assert_eq!(
        sample(&before, "potluck_db_queue_depth"),
        Some(0.0),
        "{before}"
    );
    // The first check records 9.0.0 (found); the second sees nothing newer (current).
    for _ in 0..2 {
        let response = AdminClient::connect(running.admin_socket())
            .await
            .unwrap()
            .send(Verb::Post, "/api/v1/update/check", None)
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{}", response.text());
    }
    let after = scrape(&running).await;
    assert_eq!(
        sample(&after, "potluck_update_available"),
        Some(1.0),
        "{after}"
    );
    for (outcome, want) in [
        ("found", 1.0),
        ("current", 1.0),
        ("not_found", 0.0),
        ("failed", 0.0),
    ] {
        let series = format!("potluck_update_checks_total{{outcome=\"{outcome}\"}}");
        assert_eq!(sample(&after, &series), Some(want), "{series} in {after}");
    }
    running.shutdown().await;
}

const PUSH_TOKEN: &str = "plk_metrics_push_token_000000000000000000000";
const CANARY: &str = "sk_live_canary_7f3a9c2e1b";

async fn scrape_at(addr: SocketAddr) -> String {
    client()
        .get(format!("http://{addr}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// Scrapes until `series` is present, since the reaper records after the client has its answer.
async fn scrape_until(addr: SocketAddr, series: &str) -> String {
    for _ in 0..200 {
        let text = scrape_at(addr).await;
        if sample(&text, series).is_some() {
            return text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("{series} never appeared");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_secret_push_is_counted_without_leaking_anything() {
    let access = TestAccess::new()
        .workspace("acme", "ledger")
        .grant(PUSH_TOKEN, "acme", "ledger", Role::Write)
        .hook_env("POTLUCK_KIND", "apinata")
        .push_context(PushContext::new(1, "alice").kind("apinata"));
    let server = git_server(access, None, &["--metrics-listen", "127.0.0.1:0"]).await;
    server
        .repos
        .init(
            &Slug::parse("acme").unwrap(),
            &Slug::parse("ledger").unwrap(),
            "main",
        )
        .await
        .unwrap();
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("acme", "ledger");
    let work = work_tree(&home, "work");
    std::fs::write(work.join("opencollection.yml"), APINATA_FORMAT).unwrap();
    ok(git(&home, Some(&work), None, &["add", "-A"]));
    ok(git(
        &home,
        Some(&work),
        None,
        &["commit", "-q", "-m", "format"],
    ));
    ok(git(
        &home,
        Some(&work),
        Some(PUSH_TOKEN),
        &["push", "--porcelain", &url, "main"],
    ));
    std::fs::write(work.join(".env"), format!("API_KEY={CANARY}\n")).unwrap();
    ok(git(&home, Some(&work), None, &["add", "-A"]));
    ok(git(
        &home,
        Some(&work),
        None,
        &["commit", "-q", "-m", "env"],
    ));
    let out = failed(git(
        &home,
        Some(&work),
        Some(PUSH_TOKEN),
        &["push", "--porcelain", &url, "main"],
    ));
    assert!(out.contains("potluck-reject secret_env_file"), "{out}");

    let refusal =
        "potluck_push_refusals_total{code=\"secret_env_file\",rule=\"env_file\",kind=\"apinata\"}";
    let text = scrape_until(server.metrics.expect("metrics listener"), refusal).await;
    assert_eq!(sample(&text, refusal), Some(1.0), "{text}");
    assert_eq!(
        sample(&text, "potluck_pushes_total{outcome=\"refused\"}"),
        Some(1.0),
        "{text}"
    );
    assert_eq!(
        sample(&text, "potluck_pushes_total{outcome=\"accepted\"}"),
        Some(1.0),
        "{text}"
    );
    // receive-pack exits 0 after a hook refusal, so both pushes are ok git requests.
    let rp =
        |o: &str| format!("potluck_git_requests_total{{service=\"receive_pack\",outcome=\"{o}\"}}");
    assert_eq!(sample(&text, &rp("ok")), Some(2.0), "{text}");
    let route = "http_route=\"/git/{team}/{repo}/git-receive-pack\"";
    assert!(text.contains(route), "{text}");
    for leak in [CANARY, PUSH_TOKEN, "acme", "ledger", "alice", ".env"] {
        assert!(!text.contains(leak), "{leak} leaked: {text}");
    }
}
