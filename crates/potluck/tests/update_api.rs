//! `GET /api/v1/update` and `POST /api/v1/update/check` on a started server with a loopback
//! release source.

use potluck::api::AppState;
use potluck::server::{Running, start_with};
use potluck::update::UpdateChecker;
use reqwest::Method;
use serde_json::json;

use crate::common::release::{DIGEST, ReleaseServer, TestKey, manifest};
use crate::common::{api_admin, api_user, call, fast_passwords, migrated_db, parse_config};

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct App {
    _dir: tempfile::TempDir,
    db: potluck::db::Db,
    addr: std::net::SocketAddr,
    _running: Running,
}

impl App {
    fn url(&self, path: &str) -> String {
        format!("http://{}/api/v1{path}", self.addr)
    }
}

/// A started server whose config adds `flags`, holding `checker` when given.
async fn app(flags: &[&str], checker: Option<UpdateChecker>) -> App {
    let (dir, db) = migrated_db().await;
    let data = dir.path().to_str().unwrap().to_owned();
    let mut args = vec![
        "--listen",
        "127.0.0.1:0",
        "--data",
        data.as_str(),
        "--public-url",
        "http://127.0.0.1:8080",
    ];
    args.extend_from_slice(flags);
    let config = parse_config(&args).unwrap();
    let mut state = AppState::new(db.clone(), config.clone()).with_passwords(fast_passwords());
    if let Some(checker) = checker {
        state = state.with_updates(checker);
    }
    let running = start_with(&config, state).await.expect("start");
    App {
        _dir: dir,
        db,
        addr: running.http_addr(),
        _running: running,
    }
}

async fn published(version: &str) -> (ReleaseServer, UpdateChecker) {
    let (server, key) = (ReleaseServer::start().await, TestKey::new());
    server.publish(&key, &manifest(version));
    let checker = UpdateChecker::new(server.source(&key), VERSION).unwrap();
    (server, checker)
}

fn offered(checked_at: &str, check: &str) -> serde_json::Value {
    json!({
        "current": VERSION,
        "latest": {
            "version": "9.0.0",
            "digest": DIGEST,
            "published": "2026-11-20T10:00:00Z",
            "notes_url": "https://github.com/PhongGuy/potluck/releases/tag/v9.0.0",
        },
        "checked_at": checked_at,
        "check": check,
    })
}

#[tokio::test]
async fn both_endpoints_are_for_server_admins() {
    let (server, checker) = published("9.0.0").await;
    let app = app(&[], Some(checker)).await;
    let (_, user) = api_user(&app.db, "alice").await;
    for (method, path) in [(Method::GET, "/update"), (Method::POST, "/update/check")] {
        let (status, _) = call(&app.url(path), method.clone(), None, None).await;
        assert_eq!(status, 401, "{method} {path} anonymous");
        let (status, _) = call(&app.url(path), method.clone(), Some(&user), None).await;
        assert_eq!(status, 403, "{method} {path} plain user");
    }
    assert_eq!(
        server.manifest_hits(),
        0,
        "a refused caller never triggers a fetch"
    );
}

#[tokio::test]
async fn get_reports_the_daily_schedule_and_post_checks_now() {
    let (_server, checker) = published("9.0.0").await;
    let app = app(&[], Some(checker)).await;
    let admin = api_admin(&app.db).await;
    let (status, body) = call(&app.url("/update"), Method::GET, Some(&admin), None).await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({"current": VERSION, "latest": null, "checked_at": null, "check": "daily"})
    );
    let (status, body) = call(&app.url("/update/check"), Method::POST, Some(&admin), None).await;
    assert_eq!(status, 200, "{body}");
    let checked_at = body["checked_at"]
        .as_str()
        .expect("checked_at set")
        .to_owned();
    assert_eq!(body, offered(&checked_at, "daily"));
    let (_, again) = call(&app.url("/update"), Method::GET, Some(&admin), None).await;
    assert_eq!(again, body, "GET reads what the check left");
}

#[tokio::test]
async fn no_update_check_turns_the_schedule_off_but_post_still_checks() {
    let (_server, checker) = published("9.0.0").await;
    let app = app(&["--no-update-check"], Some(checker)).await;
    let admin = api_admin(&app.db).await;
    let (_, body) = call(&app.url("/update"), Method::GET, Some(&admin), None).await;
    assert_eq!(body["check"], "off");
    let (status, body) = call(&app.url("/update/check"), Method::POST, Some(&admin), None).await;
    assert_eq!(status, 200, "{body}");
    let checked_at = body["checked_at"]
        .as_str()
        .expect("checked_at set")
        .to_owned();
    assert_eq!(body, offered(&checked_at, "off"));
}

#[tokio::test]
async fn without_a_source_the_check_is_off_and_post_is_unavailable() {
    let app = app(&[], None).await;
    let admin = api_admin(&app.db).await;
    let (_, body) = call(&app.url("/update"), Method::GET, Some(&admin), None).await;
    assert_eq!(
        body,
        json!({"current": VERSION, "latest": null, "checked_at": null, "check": "off"})
    );
    let (status, body) = call(&app.url("/update/check"), Method::POST, Some(&admin), None).await;
    assert_eq!(status, 503);
    assert_eq!(body["error"]["code"], "unavailable");
    assert_eq!(body["error"]["message"], "no update source is configured");
}

#[tokio::test]
async fn a_failed_check_is_503_naming_its_class_and_changes_nothing() {
    let (server, key) = (ReleaseServer::start().await, TestKey::new());
    server.publish(&TestKey::new(), &manifest("9.0.0"));
    let checker = UpdateChecker::new(server.source(&key), VERSION).unwrap();
    let app = app(&[], Some(checker)).await;
    let admin = api_admin(&app.db).await;
    let (status, body) = call(&app.url("/update/check"), Method::POST, Some(&admin), None).await;
    assert_eq!(status, 503);
    assert_eq!(body["error"]["code"], "unavailable");
    assert_eq!(body["error"]["message"], "signature by an unknown key");
    assert!(
        !body.to_string().contains("9.0.0"),
        "no fetched bytes in the answer: {body}"
    );
    let (_, body) = call(&app.url("/update"), Method::GET, Some(&admin), None).await;
    assert_eq!(body["latest"], serde_json::Value::Null);
    assert_eq!(body["checked_at"], serde_json::Value::Null);
}
