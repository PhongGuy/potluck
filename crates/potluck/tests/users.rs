//! User management by server admins.

use serde_json::{Value, json};

use crate::common::{TestApp, client, config_for, create_user, login, test_app, unix_get};

pub(crate) async fn get_users(app: &TestApp, token: Option<&str>) -> (u16, Value) {
    let mut req = client().get(app.url("/users"));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let r = req.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

async fn post_user(app: &TestApp, token: &str, body: Value) -> (u16, Value) {
    let r = client()
        .post(app.url("/users"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

fn new_user(username: &str) -> Value {
    json!({"username": username, "display_name": "Carol", "email": "c@example.com", "password": "carol-pw-1"})
}

#[tokio::test]
async fn only_server_admins_manage_users() {
    let app = test_app().await;
    create_user(&app.db, "bob", "pw-123456", false).await;
    create_user(&app.db, "alice", "pw-123456", true).await;
    let bob = login(&app, "bob", "pw-123456").await;
    let alice = login(&app, "alice", "pw-123456").await;
    let (status, body) = get_users(&app, None).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (401, Some("unauthenticated")),
        "{body}"
    );
    let (status, body) = get_users(&app, Some(&bob)).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (403, Some("forbidden")),
        "{body}"
    );
    let (status, body) = get_users(&app, Some(&alice)).await;
    assert_eq!(status, 200, "{body}");
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["username"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["alice", "bob"]);
    assert!(
        body[0].get("password_hash").is_none() && body[0].get("password").is_none(),
        "{body}"
    );
    assert_eq!(post_user(&app, &bob, new_user("carol")).await.0, 403);
}

#[tokio::test]
async fn an_admin_creates_a_user_who_can_sign_in() {
    let app = test_app().await;
    create_user(&app.db, "alice", "pw-123456", true).await;
    let alice = login(&app, "alice", "pw-123456").await;
    let (status, body) = post_user(&app, &alice, new_user("carol")).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["username"], "carol");
    assert_eq!(body["display_name"], "Carol");
    assert_eq!(body["email"], "c@example.com");
    assert_eq!(body["server_admin"], false);
    assert_eq!(body["disabled"], false);
    assert_eq!(body["created_at"].as_str().unwrap().len(), 20);
    assert!(body.get("password").is_none(), "{body}");
    login(&app, "carol", "carol-pw-1").await;
    let (status, body) = post_user(&app, &alice, new_user("carol")).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (409, Some("conflict")),
        "{body}"
    );
    let (status, body) = post_user(&app, &alice, json!({"username": "dave", "display_name": "Dave", "password": "dave-pw-123", "server_admin": true})).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["server_admin"], true);
    assert_eq!(body["email"], Value::Null);
}

#[tokio::test]
async fn new_users_are_validated() {
    let app = test_app().await;
    create_user(&app.db, "alice", "pw-123456", true).await;
    let alice = login(&app, "alice", "pw-123456").await;
    let cases = [
        (
            json!({"username": "system", "display_name": "S", "password": "sys-pw-1234"}),
            "username",
        ),
        (
            json!({"username": "Carol", "display_name": "C", "password": "carol-pw-1"}),
            "username",
        ),
        (
            json!({"username": "-carol", "display_name": "C", "password": "carol-pw-1"}),
            "username",
        ),
        (
            json!({"username": "a".repeat(64), "display_name": "C", "password": "carol-pw-1"}),
            "username",
        ),
        (
            json!({"username": "carol", "display_name": "  ", "password": "carol-pw-1"}),
            "display_name",
        ),
        (
            json!({"username": "carol", "display_name": "x".repeat(201), "password": "carol-pw-1"}),
            "display_name",
        ),
        (
            json!({"username": "carol", "display_name": "C", "password": "carol-pw-1", "email": "nope"}),
            "email",
        ),
        (
            json!({"username": "carol", "display_name": "C", "password": "carol-pw-1", "email": "@x"}),
            "email",
        ),
        (
            json!({"username": "carol", "display_name": "C", "password": "short"}),
            "password",
        ),
        (
            json!({"username": "carol", "display_name": "C", "password": "x".repeat(1025)}),
            "password",
        ),
    ];
    for (body, field) in cases {
        let (status, err) = post_user(&app, &alice, body.clone()).await;
        assert_eq!(status, 422, "{body}: {err}");
        assert_eq!(err["error"]["field"], field, "{body}: {err}");
    }
    let (status, _) = post_user(
        &app,
        &alice,
        json!({"username": "carol", "display_name": "C"}),
    )
    .await;
    assert_eq!(status, 400, "password is required");
    let (status, body) = post_user(
        &app,
        &alice,
        json!({"username": "a".repeat(63), "display_name": "C", "password": "carol-pw-1"}),
    )
    .await;
    assert_eq!(status, 201, "63 characters is the longest name: {body}");
}

#[tokio::test]
async fn the_admin_socket_lists_users_as_system() {
    let dir = tempfile::tempdir().unwrap();
    let running = potluck::server::start(&config_for(dir.path()))
        .await
        .unwrap();
    let (status, body) = unix_get(running.admin_socket(), "/api/v1/users").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), json!([]));
    running.shutdown().await;
}
