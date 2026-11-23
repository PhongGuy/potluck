//! The metrics listener: off by default, OpenMetrics on `/metrics`, nothing else.

use std::net::SocketAddr;

use potluck::api::AppState;
use potluck::db::Db;
use potluck::server::{Running, ServerError, start, start_with};

use crate::common::{client, config_for, fast_passwords, parse_config};

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
