//! `GET /api/v1/audit`: who may read it, filters, paging and validation.

use potluck::clock::{now_rfc3339, shift_secs};
use reqwest::Method;
use serde_json::Value;

use crate::common::{TestApp, api_admin, api_user, call, test_app};

/// Inserts `n` rows a minute apart from ten days ago (actor bob, workspace core/api for even n)
/// through a second connection on the app's database file, and returns their stamps in order.
/// Recent stamps keep the rows clear of the startup pruner's retention cutoff.
fn seed(app: &TestApp, n: i64) -> Vec<String> {
    let c = rusqlite::Connection::open(app._dir.path().join("server.db")).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    let base = shift_secs(&now_rfc3339(), -10 * 86_400).unwrap();
    let mut stamps = Vec::new();
    for i in 0..n {
        let at = shift_secs(&base, 60 * i).unwrap();
        let ws = (i % 2 == 0).then_some("core/api");
        c.execute(
            "INSERT INTO audit (at, action, actor_kind, actor_id, actor_name, target_kind, \
             target_name, workspace, client_ip, detail) \
             VALUES (?1, 'grant.put', 'user', 3, 'bob', 'user', 'ann', ?2, '192.0.2.9', \
             '{\"old\":null,\"new\":\"read\"}')",
            rusqlite::params![at, ws],
        )
        .unwrap();
        stamps.push(at);
    }
    stamps
}

async fn get(app: &TestApp, token: &str, query: &str) -> (u16, Value) {
    call(
        &app.url(&format!("/audit{query}")),
        Method::GET,
        Some(token),
        None,
    )
    .await
}

#[tokio::test]
async fn only_server_admins_read_it() {
    let app = test_app().await;
    let (_, bob) = api_user(&app.db, "bob").await;
    let (status, _) = call(&app.url("/audit"), Method::GET, None, None).await;
    assert_eq!(status, 401);
    let (status, body) = get(&app, &bob, "").await;
    assert_eq!(status, 403, "{body}");
    let root = api_admin(&app.db).await;
    let (status, body) = get(&app, &root, "").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.is_array());
}

#[tokio::test]
async fn a_row_has_every_field() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let stamps = seed(&app, 1);
    let (status, body) = get(&app, &root, "?action=grant.put").await;
    assert_eq!(status, 200, "{body}");
    let row = &body[0];
    assert_eq!(row["action"], "grant.put");
    assert_eq!(row["at"], stamps[0].as_str());
    assert_eq!(row["actor_kind"], "user");
    assert_eq!(row["actor_id"], 3);
    assert_eq!(row["actor_name"], "bob");
    assert_eq!(row["target_kind"], "user");
    assert_eq!(row["target_id"], Value::Null);
    assert_eq!(row["target_name"], "ann");
    assert_eq!(row["workspace"], "core/api");
    assert_eq!(row["client_ip"], "192.0.2.9");
    assert_eq!(
        row["detail"],
        serde_json::json!({"old": null, "new": "read"})
    );
    assert!(row["id"].is_i64());
}

#[tokio::test]
async fn paging_with_before_walks_every_row_once() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let stamps = seed(&app, 250);
    let mut rows = Vec::new();
    // Seeded rows are the only grant.put rows, whatever setup writes once actions are audited.
    let mut query = "?limit=100&action=grant.put".to_owned();
    loop {
        let (status, body) = get(&app, &root, &query).await;
        assert_eq!(status, 200, "{body}");
        let page = body.as_array().unwrap();
        let Some(last) = page.last() else { break };
        rows.extend(page.iter().map(|r| {
            (
                r["id"].as_i64().unwrap(),
                r["at"].as_str().unwrap().to_owned(),
            )
        }));
        query = format!("?limit=100&action=grant.put&before={}", last["id"]);
    }
    assert_eq!(rows.len(), 250);
    assert!(
        rows.windows(2).all(|w| w[0].0 > w[1].0),
        "newest first, no repeats"
    );
    let served: Vec<_> = rows.into_iter().rev().map(|(_, at)| at).collect();
    assert_eq!(served, stamps, "every seeded stamp, in insertion order");
}

#[tokio::test]
async fn filters_and_an_offset_stamp() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let stamps = seed(&app, 120);
    let (_, body) = get(&app, &root, "?workspace=core%2Fapi&limit=1000").await;
    assert_eq!(body.as_array().unwrap().len(), 60);
    let (_, body) = get(&app, &root, "?actor=ann").await;
    assert_eq!(body.as_array().unwrap().len(), 0);
    let (_, body) = get(&app, &root, "?action=grant.put&limit=5").await;
    assert_eq!(body.as_array().unwrap().len(), 5);
    // The first stamp written at +01:00 is the same instant, so since/until cover rows 0..30.
    let first: jiff::Timestamp = stamps[0].parse().unwrap();
    let since = format!(
        "{:.0}",
        first.display_with_offset(jiff::tz::Offset::constant(1))
    );
    assert!(since.ends_with("+01:00"), "{since}");
    let query = format!(
        "?since={}&until={}&limit=1000",
        since.replace('+', "%2B"),
        stamps[30]
    );
    let (status, body) = get(&app, &root, &query).await;
    assert_eq!(status, 200, "{body}");
    let ats: Vec<_> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["at"].as_str().unwrap().to_owned())
        .rev()
        .collect();
    assert_eq!(ats, stamps[..30]);
    let (status, _) = get(&app, &root, "?kind=whatever").await;
    assert_eq!(status, 200, "unknown parameters are ignored");
}

#[tokio::test]
async fn bad_values_are_422_naming_the_field() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    for (query, field) in [
        ("?since=yesterday", "since"),
        ("?until=2026-13-01T00:00:00Z", "until"),
        ("?before=abc", "before"),
        ("?limit=0", "limit"),
        ("?limit=1001", "limit"),
        ("?limit=x", "limit"),
    ] {
        let (status, body) = get(&app, &root, query).await;
        assert_eq!(status, 422, "{query}: {body}");
        assert_eq!(body["error"]["field"], field, "{query}: {body}");
    }
}
