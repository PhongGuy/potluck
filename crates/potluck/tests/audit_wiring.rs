//! Each mutating call writes exactly one audit row in its own transaction, or none.

use std::path::Path;

use potluck::db::Db;
use potluck::db::audit::{AuditCtx, AuditFilter, AuditRecord};
use reqwest::Method;
use serde_json::{Value, json};

use crate::common::{api_admin, api_user, call, test_app};

/// The rows of `action`, oldest first.
async fn rows(db: &Db, action: &str) -> Vec<AuditRecord> {
    let mut rows = db
        .audit(AuditFilter::new().action(action).limit(1000))
        .await
        .unwrap();
    rows.reverse();
    rows
}

fn detail(row: &AuditRecord) -> Value {
    serde_json::from_str(row.detail().expect("a detail")).unwrap()
}

/// From here on every audit insert in `<data>/server.db` fails.
fn boom(data: &Path) {
    let c = rusqlite::Connection::open(data.join("server.db")).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    c.execute_batch(
        "CREATE TRIGGER boom BEFORE INSERT ON audit BEGIN SELECT RAISE(ABORT, 'boom'); END",
    )
    .unwrap();
}

#[tokio::test]
async fn creating_and_patching_a_user_writes_one_row_each() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let root_id = app.db.user_by_username("root").await.unwrap().unwrap().id();
    let (status, _) = call(
        &app.url("/users"),
        Method::POST,
        Some(&root),
        Some(json!({"username": "bob", "display_name": "Bob", "password": "pw-123456"})),
    )
    .await;
    assert_eq!(status, 201);
    let created = rows(&app.db, "user.create").await;
    let row = created.last().unwrap();
    assert_eq!(
        (row.actor_kind(), row.actor_id(), row.actor_name()),
        ("user", Some(root_id), Some("root"))
    );
    assert_eq!(
        (row.target_kind(), row.target_name()),
        (Some("user"), Some("bob"))
    );
    assert_eq!(row.client_ip(), Some("127.0.0.1"));
    assert_eq!(detail(row), json!({"server_admin": false}));

    let (status, _) = call(
        &app.url("/users/bob"),
        Method::PATCH,
        Some(&root),
        Some(json!({"display_name": "Robert", "disabled": true, "password": "pw-654321"})),
    )
    .await;
    assert_eq!(status, 200);
    let updated = rows(&app.db, "user.update").await;
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].target_name(), Some("bob"));
    assert_eq!(
        detail(&updated[0]),
        json!({"fields": ["display_name", "disabled", "password"], "disabled": true})
    );

    let (status, _) = call(
        &app.url("/users/bob"),
        Method::PATCH,
        Some(&root),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = call(
        &app.url("/users/ghost"),
        Method::PATCH,
        Some(&root),
        Some(json!({"display_name": "G"})),
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(
        rows(&app.db, "user.update").await.len(),
        1,
        "no-ops write nothing"
    );
}

#[tokio::test]
async fn the_system_actor_has_no_id_and_no_address() {
    let app = test_app().await;
    app.db
        .create_user(
            potluck::db::accounts::NewUser::new("eve", "Eve"),
            &AuditCtx::system(),
        )
        .await
        .unwrap();
    let row = rows(&app.db, "user.create").await.pop().unwrap();
    assert_eq!(
        (row.actor_kind(), row.actor_id(), row.actor_name()),
        ("system", None, Some("system"))
    );
    assert_eq!(row.client_ip(), None);
}

#[tokio::test]
async fn a_failed_insert_rolls_the_user_change_back() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    boom(app._dir.path());
    let (status, _) = call(
        &app.url("/users"),
        Method::POST,
        Some(&root),
        Some(json!({"username": "carol", "display_name": "Carol", "password": "pw-123456"})),
    )
    .await;
    assert_eq!(status, 500);
    assert!(
        app.db.user_by_username("carol").await.unwrap().is_none(),
        "no user without its row"
    );
    let err = app
        .db
        .update_user(
            "root",
            potluck::db::accounts::UserPatch::default().display_name("R"),
            &AuditCtx::system(),
        )
        .await;
    assert!(err.is_err());
    let root_row = app.db.user_by_username("root").await.unwrap().unwrap();
    assert_eq!(root_row.display_name(), "root", "the patch rolled back");
}

#[tokio::test]
async fn signing_in_writes_session_create_and_no_token_create() {
    let app = test_app().await;
    let (bob, _) = api_user(&app.db, "bob").await;
    let before = rows(&app.db, "token.create").await.len();
    let (status, body) = call(
        &app.url("/session"),
        Method::POST,
        None,
        Some(json!({"username": "bob", "password": "pw-123456", "token_name": "desk"})),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let row = rows(&app.db, "session.create").await.pop().unwrap();
    assert_eq!((row.actor_kind(), row.actor_id()), ("user", Some(bob.id())));
    assert_eq!(
        (row.target_kind(), row.target_id(), row.target_name()),
        (
            Some("token"),
            body["token_meta"]["id"].as_i64(),
            Some("desk")
        )
    );
    assert_eq!(row.client_ip(), Some("127.0.0.1"));
    assert_eq!(row.detail(), None);
    assert_eq!(
        rows(&app.db, "token.create").await.len(),
        before,
        "one row per minted token"
    );
}

#[tokio::test]
async fn creating_and_revoking_a_token_names_its_owner() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let (_, bob) = api_user(&app.db, "bob").await;
    let (status, body) = call(
        &app.url("/tokens"),
        Method::POST,
        Some(&bob),
        Some(json!({"name": "ci"})),
    )
    .await;
    assert_eq!(status, 201);
    let id = body["token_meta"]["id"].as_i64().unwrap();
    let row = rows(&app.db, "token.create").await.pop().unwrap();
    assert_eq!(
        (row.actor_name(), row.target_id(), row.target_name()),
        (Some("bob"), Some(id), Some("ci"))
    );
    assert_eq!(detail(&row), json!({"user": "bob"}));

    let (status, _) = call(
        &app.url(&format!("/tokens/{id}")),
        Method::DELETE,
        Some(&root),
        None,
    )
    .await;
    assert_eq!(status, 204);
    let (status, _) = call(
        &app.url(&format!("/tokens/{id}")),
        Method::DELETE,
        Some(&root),
        None,
    )
    .await;
    assert_eq!(status, 404);
    let revoked = rows(&app.db, "token.revoke").await;
    assert_eq!(revoked.len(), 1, "the 404 writes nothing");
    assert_eq!(
        (revoked[0].actor_name(), revoked[0].target_name()),
        (Some("root"), Some("ci"))
    );
    assert_eq!(detail(&revoked[0]), json!({"user": "bob"}));
}

#[tokio::test]
async fn a_failed_insert_mints_no_token() {
    let app = test_app().await;
    let (bob_user, bob) = api_user(&app.db, "bob").await;
    let count = app.db.tokens_for_user(bob_user.id()).await.unwrap().len();
    boom(app._dir.path());
    let (status, _) = call(
        &app.url("/tokens"),
        Method::POST,
        Some(&bob),
        Some(json!({"name": "ci"})),
    )
    .await;
    assert_eq!(status, 500);
    assert_eq!(
        app.db.tokens_for_user(bob_user.id()).await.unwrap().len(),
        count
    );
}
