//! `/api/v1/health` and `/api/v1/server` over loopback TCP, and the router's fallbacks.

use potluck::api::AppState;
use potluck::server::{Running, start_with};
use serde_json::{Value, json};

use crate::common::{config_for, migrated_db};

async fn get(addr: std::net::SocketAddr, path: &str) -> (u16, Value) {
    let response = reqwest::get(format!("http://{addr}{path}")).await.unwrap();
    (response.status().as_u16(), response.json().await.unwrap())
}

/// A started server over a migrated database; the dir keeps the files alive.
async fn app() -> (
    tempfile::TempDir,
    potluck::db::Db,
    std::net::SocketAddr,
    Running,
) {
    let (dir, db) = migrated_db().await;
    let config = config_for(dir.path());
    let running = start_with(&config, AppState::new(db.clone(), config.clone()))
        .await
        .expect("start");
    (dir, db, running.http_addr(), running)
}

#[tokio::test]
async fn health_reports_ok_and_the_version() {
    let (_dir, _db, addr, _task) = app().await;
    let (status, body) = get(addr, "/api/v1/health").await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")})
    );
}

#[tokio::test]
async fn server_describes_api_1_with_password_auth_only() {
    let (_dir, _db, addr, _task) = app().await;
    let (status, body) = get(addr, "/api/v1/server").await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({"version": env!("CARGO_PKG_VERSION"), "api": 1, "auth": {"password": true, "oidc": null},
               "kinds": {"apinata": {"formats": ["1.0.0"]}, "dbento": {"formats": ["1"]}}})
    );
}

#[tokio::test]
async fn unknown_paths_answer_the_json_error_model() {
    let (_dir, _db, addr, _task) = app().await;
    for path in ["/api/v1/nope", "/nope", "/api/v2/health"] {
        let (status, body) = get(addr, path).await;
        assert_eq!(status, 404, "{path}");
        assert_eq!(body["error"]["code"], "not_found", "{path}");
    }
}

#[tokio::test]
async fn a_wrong_method_answers_not_found() {
    let (_dir, _db, addr, _task) = app().await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/api/v1/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 404);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn health_is_503_unavailable_when_the_database_is_down() {
    let (_dir, db, addr, _task) = app().await;
    db.close();
    let (status, body) = get(addr, "/api/v1/health").await;
    assert_eq!(status, 503);
    assert_eq!(body["error"]["code"], "unavailable");
}
