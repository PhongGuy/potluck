//! `POST /session`: minting, the one 401, validation, and both rate limits.

use std::collections::HashSet;

use potluck::db::accounts::{NewUser, UserPatch};
use potluck::db::audit::AuditCtx;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};

use crate::common::{TestApp, client, create_user, login, test_app};

async fn post_session(app: &TestApp, body: Value) -> (u16, HeaderMap, Value) {
    let r = client()
        .post(app.url("/session"))
        .json(&body)
        .send()
        .await
        .unwrap();
    (
        r.status().as_u16(),
        r.headers().clone(),
        r.json().await.unwrap(),
    )
}

fn creds(username: &str, password: &str) -> Value {
    json!({"username": username, "password": password, "token_name": "laptop"})
}

#[tokio::test]
async fn a_correct_password_mints_a_token() {
    let app = test_app().await;
    let bob = create_user(&app.db, "bob", "pw-123456", false).await;
    let (status, _, body) = post_session(&app, creds("bob", "pw-123456")).await;
    assert_eq!(status, 201, "{body}");
    let token = body["token"].as_str().unwrap();
    assert_eq!(token.len(), 47);
    assert!(token.starts_with("plk_"));
    assert_eq!(body["token_meta"]["name"], "laptop");
    assert_eq!(body["token_meta"]["prefix"], &token[..8]);
    assert_eq!(body["token_meta"]["expires_at"], Value::Null);
    assert_eq!(body["token_meta"]["last_used_at"], Value::Null);
    assert!(body["token_meta"]["id"].is_i64(), "{body}");
    assert_eq!(body["token_meta"]["created_at"].as_str().unwrap().len(), 20);
    assert_eq!(
        body["user"],
        json!({"username": "bob", "display_name": "bob", "email": null, "server_admin": false, "disabled": false, "created_at": bob.created_at()})
    );
    let stored = app.db.tokens_for_user(bob.id()).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].prefix(), &token[..8]);
    assert_eq!(
        login(&app, "bob", "pw-123456").await.len(),
        47,
        "the helper signs in too"
    );
}

#[tokio::test]
async fn wrong_password_unknown_user_and_disabled_user_are_401_alike() {
    let app = test_app().await;
    let bob = create_user(&app.db, "bob", "pw-123456", false).await;
    create_user(&app.db, "dan", "pw-123456", false).await;
    app.db
        .update_user(
            "dan",
            UserPatch::default().disabled(true),
            &AuditCtx::system(),
        )
        .await
        .unwrap();
    let mut messages = HashSet::new();
    for (u, p) in [
        ("bob", "wrong-pw"),
        ("nobody", "pw-123456"),
        ("dan", "pw-123456"),
    ] {
        let (status, headers, body) = post_session(&app, creds(u, p)).await;
        assert_eq!(status, 401, "{u}: {body}");
        assert_eq!(body["error"]["code"], "unauthenticated");
        assert_eq!(headers["www-authenticate"], "Basic realm=\"potluck\"");
        let text = body.to_string();
        assert!(
            !text.contains("pw-123456") && !text.contains("wrong-pw"),
            "{body}"
        );
        messages.insert(body["error"]["message"].as_str().unwrap().to_owned());
    }
    assert_eq!(messages.len(), 1, "one message for all three: {messages:?}");
    assert!(app.db.tokens_for_user(bob.id()).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_user_without_a_password_cannot_sign_in() {
    let app = test_app().await;
    app.db
        .create_user(NewUser::new("sso", "SSO Only"), &AuditCtx::system())
        .await
        .unwrap();
    let (status, _, body) = post_session(&app, creds("sso", "")).await;
    assert_eq!(status, 401, "{body}");
    let (status, _, _) = post_session(&app, creds("sso", "anything")).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn the_body_is_validated() {
    let app = test_app().await;
    let (status, _, body) = post_session(&app, json!({"username": "bob", "password": "pw"})).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("token_name"),
        "{body}"
    );
    let (status, _, body) = post_session(
        &app,
        json!({"username": "bob", "password": "pw", "token_name": "   "}),
    )
    .await;
    assert_eq!(status, 422);
    assert_eq!(body["error"]["field"], "token_name");
    let (status, _, body) = post_session(
        &app,
        json!({"username": "bob", "password": "pw", "token_name": "x".repeat(101)}),
    )
    .await;
    assert_eq!(status, 422, "{body}");
}

#[tokio::test]
async fn too_many_attempts_for_one_username_are_429_with_retry_after() {
    let app = test_app().await;
    create_user(&app.db, "bob", "pw-123456", false).await;
    for i in 0..5 {
        let (status, _, body) = post_session(&app, creds("bob", "wrong-pw")).await;
        assert_eq!(status, 401, "attempt {i}: {body}");
    }
    let (status, headers, body) = post_session(&app, creds("bob", "pw-123456")).await;
    assert_eq!(status, 429, "{body}");
    assert_eq!(body["error"]["code"], "rate_limited");
    let retry: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry), "{retry}");
    let (status, _, _) = post_session(&app, creds("alice", "x")).await;
    assert_eq!(status, 401, "another username has its own window");
}

#[tokio::test]
async fn too_many_attempts_from_one_address_are_429() {
    let app = test_app().await;
    for i in 0..10 {
        let (status, _, body) = post_session(&app, creds(&format!("u{i}"), "x")).await;
        assert_eq!(status, 401, "attempt {i}: {body}");
    }
    let (status, _, body) = post_session(&app, creds("u99", "x")).await;
    assert_eq!(status, 429, "{body}");
}

#[tokio::test]
async fn a_rejected_token_name_counts_against_neither_limiter() {
    let app = test_app().await;
    create_user(&app.db, "bob", "pw-123456", false).await;
    // Ten 422s for one username from one address: past the username limit (5) and up to the
    // address limit (10), so counting them against either limiter turns the next sign-in into 429.
    for i in 0..10 {
        let (status, _, body) = post_session(
            &app,
            json!({"username": "bob", "password": "pw-123456", "token_name": "  "}),
        )
        .await;
        assert_eq!(status, 422, "attempt {i}: {body}");
        assert_eq!(body["error"]["field"], "token_name", "attempt {i}: {body}");
    }
    let (status, _, body) = post_session(&app, creds("bob", "pw-123456")).await;
    assert_eq!(status, 201, "a 422 counted against a login limiter: {body}");
}
