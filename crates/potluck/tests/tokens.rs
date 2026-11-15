//! Own tokens, an admin's view of a user's tokens, and `/me`.

use potluck::db::accounts::UserPatch;
use serde_json::{Value, json};

use crate::common::{
    TestApp, client, config_for, create_user, login, mint_token, test_app, unix_get,
};

async fn get(app: &TestApp, path: &str, token: Option<&str>) -> (u16, Value) {
    let mut req = client().get(app.url(path));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let r = req.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

async fn post(app: &TestApp, path: &str, token: &str, body: Value) -> (u16, Value) {
    let r = client()
        .post(app.url(path))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

async fn delete(app: &TestApp, path: &str, token: &str) -> u16 {
    client()
        .delete(app.url(path))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn self_endpoints_need_a_user() {
    let app = test_app().await;
    for path in ["/me", "/tokens"] {
        let (status, body) = get(&app, path, None).await;
        assert_eq!(status, 401, "{path}: {body}");
        assert_eq!(body["error"]["code"], "unauthenticated");
    }
    let r = client()
        .post(app.url("/tokens"))
        .json(&json!({"name": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
}

#[tokio::test]
async fn creating_listing_and_revoking_own_tokens() {
    let app = test_app().await;
    create_user(&app.db, "bob", "pw-123456", false).await;
    let laptop = login(&app, "bob", "pw-123456").await;
    let (status, created) = post(
        &app,
        "/tokens",
        &laptop,
        json!({"name": "ci", "expires_at": "2030-01-01T12:00:00+02:00"}),
    )
    .await;
    assert_eq!(status, 201, "{created}");
    let ci = created["token"].as_str().unwrap().to_owned();
    assert_eq!(ci.len(), 47);
    assert_eq!(created["token_meta"]["name"], "ci");
    assert_eq!(
        created["token_meta"]["expires_at"], "2030-01-01T10:00:00Z",
        "normalised to UTC"
    );
    let ci_id = created["token_meta"]["id"].as_i64().unwrap();
    let (status, list) = get(&app, "/tokens", Some(&laptop)).await;
    assert_eq!(status, 200);
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["test", "ci"],
        "creation order; `login` names its token `test`"
    );
    assert!(
        list[0]["last_used_at"].is_string(),
        "the laptop token was just used: {list}"
    );
    assert!(
        list[1].get("token").is_none() && list[1].get("digest").is_none(),
        "{list}"
    );
    assert_eq!(
        get(&app, "/tokens", Some(&ci)).await.0,
        200,
        "the new token works"
    );
    assert_eq!(
        delete(&app, &format!("/tokens/{ci_id}"), &laptop).await,
        204
    );
    assert_eq!(
        get(&app, "/tokens", Some(&laptop))
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(get(&app, "/tokens", Some(&ci)).await.0, 401, "revoked");
    assert_eq!(
        delete(&app, &format!("/tokens/{ci_id}"), &laptop).await,
        404,
        "already gone"
    );
    assert_eq!(delete(&app, "/tokens/abc", &laptop).await, 404);
}

#[tokio::test]
async fn token_names_and_expiry_are_validated() {
    let app = test_app().await;
    create_user(&app.db, "bob", "pw-123456", false).await;
    let t = login(&app, "bob", "pw-123456").await;
    for (body, field) in [
        (json!({"name": ""}), "name"),
        (json!({"name": "  "}), "name"),
        (json!({"name": "x".repeat(101)}), "name"),
        (
            json!({"name": "ok", "expires_at": "tomorrow"}),
            "expires_at",
        ),
        (
            json!({"name": "ok", "expires_at": "2020-01-01T00:00:00Z"}),
            "expires_at",
        ),
    ] {
        let (status, err) = post(&app, "/tokens", &t, body.clone()).await;
        assert_eq!(status, 422, "{body}: {err}");
        assert_eq!(err["error"]["code"], "invalid_request");
        assert_eq!(err["error"]["field"], field, "{body}");
    }
    let (status, ok) = post(
        &app,
        "/tokens",
        &t,
        json!({"name": "never", "expires_at": null}),
    )
    .await;
    assert_eq!(status, 201, "{ok}");
    assert_eq!(ok["token_meta"]["expires_at"], Value::Null);
    let (status, _) = post(&app, "/tokens", &t, json!({"expires_at": null})).await;
    assert_eq!(status, 400, "name is required");
}

#[tokio::test]
async fn an_expired_token_is_listed_but_refused() {
    let app = test_app().await;
    let bob = create_user(&app.db, "bob", "pw-123456", false).await;
    let old = mint_token(&app.db, &bob, "old", Some("2020-01-01T00:00:00Z")).await;
    let good = mint_token(&app.db, &bob, "good", None).await;
    assert_eq!(get(&app, "/tokens", Some(&old)).await.0, 401);
    let (_, list) = get(&app, "/tokens", Some(&good)).await;
    assert_eq!(list[0]["name"], "old");
    assert_eq!(list[0]["expires_at"], "2020-01-01T00:00:00Z");
}

#[tokio::test]
async fn only_an_admin_sees_or_revokes_another_user_s_tokens() {
    let app = test_app().await;
    let bob = create_user(&app.db, "bob", "pw-123456", false).await;
    let alice = create_user(&app.db, "alice", "pw-123456", true).await;
    let bob_t = mint_token(&app.db, &bob, "bob-1", None).await;
    let alice_t = mint_token(&app.db, &alice, "alice-1", None).await;
    let bob_id = app.db.tokens_for_user(bob.id()).await.unwrap()[0].id();
    let alice_id = app.db.tokens_for_user(alice.id()).await.unwrap()[0].id();
    let (status, list) = get(&app, "/users/bob/tokens", Some(&alice_t)).await;
    assert_eq!(status, 200, "{list}");
    assert_eq!(list[0]["name"], "bob-1");
    let (status, body) = get(&app, "/users/alice/tokens", Some(&bob_t)).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "forbidden");
    assert_eq!(
        get(&app, "/users/nobody/tokens", Some(&alice_t)).await.0,
        404
    );
    assert_eq!(get(&app, "/users/bob/tokens", None).await.0, 401);
    assert_eq!(
        delete(&app, &format!("/tokens/{alice_id}"), &bob_t).await,
        404,
        "not readable to bob"
    );
    assert_eq!(
        delete(&app, &format!("/tokens/{bob_id}"), &alice_t).await,
        204,
        "an admin revokes any token"
    );
    assert_eq!(get(&app, "/tokens", Some(&bob_t)).await.0, 401);
}

#[tokio::test]
async fn me_returns_the_user_the_identity_and_no_workspaces_yet() {
    let app = test_app().await;
    create_user(&app.db, "bob", "pw-123456", false).await;
    let t = login(&app, "bob", "pw-123456").await;
    let (status, me) = get(&app, "/me", Some(&t)).await;
    assert_eq!(status, 200, "{me}");
    assert_eq!(me["user"]["username"], "bob");
    assert_eq!(
        me["identity"],
        json!({"name": "bob", "email": "bob@users.noreply.127.0.0.1"}),
        "{me}"
    );
    assert_eq!(me["workspaces"], json!([]));
    app.db
        .update_user(
            "bob",
            UserPatch::default()
                .display_name("Bob B")
                .email(Some("bob@example.com")),
        )
        .await
        .unwrap();
    let (_, me) = get(&app, "/me", Some(&t)).await;
    assert_eq!(
        me["identity"],
        json!({"name": "Bob B", "email": "bob@example.com"})
    );
    assert_eq!(me["user"]["display_name"], "Bob B");
}

#[tokio::test]
async fn the_system_actor_has_no_self() {
    let dir = tempfile::tempdir().unwrap();
    let running = potluck::server::start(&config_for(dir.path()))
        .await
        .unwrap();
    for path in ["/api/v1/me", "/api/v1/tokens"] {
        let (status, body) = unix_get(running.admin_socket(), path).await;
        assert_eq!(status, 403, "{path}: {body}");
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
            "forbidden"
        );
    }
    running.shutdown().await;
}
