//! The client address behind the server's TCP listener, with and without trusted proxies.

use serde_json::json;

use crate::common::{client, config_for, parse_config};

/// One failed sign-in as a fresh username (so only the address limit counts), with the given
/// `X-Forwarded-For` lines; returns the status.
async fn attempt(url: &str, forwarded_for: &[&str], n: usize) -> u16 {
    let body = json!({"username": format!("probe{n}"), "password": "x", "token_name": "probe"});
    let mut req = client().post(url).json(&body);
    for line in forwarded_for {
        req = req.header("x-forwarded-for", *line);
    }
    req.send().await.unwrap().status().as_u16()
}

async fn server_trusting_loopback(dir: &std::path::Path) -> potluck::server::Running {
    let config = parse_config(&[
        "--listen",
        "127.0.0.1:0",
        "--data",
        dir.to_str().unwrap(),
        "--public-url",
        "http://127.0.0.1:8080",
        "--trusted-proxies",
        "127.0.0.1",
    ])
    .unwrap();
    potluck::server::start(&config).await.unwrap()
}

fn session_url(running: &potluck::server::Running) -> String {
    format!("http://{}/api/v1/session", running.http_addr())
}

#[tokio::test]
async fn behind_a_trusted_proxy_the_forwarded_address_is_the_client() {
    let dir = tempfile::tempdir().unwrap();
    let running = server_trusting_loopback(dir.path()).await;
    let url = session_url(&running);
    for i in 0..10 {
        assert_eq!(attempt(&url, &["198.51.100.1"], i).await, 401, "{i}");
    }
    assert_eq!(
        attempt(&url, &["198.51.100.1"], 10).await,
        429,
        "the forwarded address is exhausted"
    );
    assert_eq!(
        attempt(&url, &["198.51.100.2"], 11).await,
        401,
        "another forwarded address has its own window"
    );
    running.shutdown().await;
}

#[tokio::test]
async fn a_second_header_line_from_the_proxy_wins() {
    let dir = tempfile::tempdir().unwrap();
    let running = server_trusting_loopback(dir.path()).await;
    let url = session_url(&running);
    // The client varies its own line; the proxy's appended line names the real client.
    for i in 0..10 {
        let attacker = format!("203.0.113.{i}");
        assert_eq!(
            attempt(&url, &[attacker.as_str(), "198.51.100.1"], i).await,
            401,
            "{i}"
        );
    }
    assert_eq!(
        attempt(&url, &["203.0.113.99", "198.51.100.1"], 10).await,
        429,
        "keyed on the proxy's line"
    );
    running.shutdown().await;
}

#[tokio::test]
async fn without_trusted_proxies_the_header_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let running = potluck::server::start(&config_for(dir.path()))
        .await
        .unwrap();
    let url = session_url(&running);
    for i in 0..10 {
        let forged = format!("198.51.100.{i}");
        assert_eq!(attempt(&url, &[forged.as_str()], i).await, 401, "{i}");
    }
    assert_eq!(
        attempt(&url, &["198.51.100.200"], 10).await,
        429,
        "every request is the loopback peer"
    );
    running.shutdown().await;
}
