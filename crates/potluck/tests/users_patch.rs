//! Self-service profile and password changes, and admin edits.

use serde_json::{Value, json};

use crate::common::{TestApp, client, create_user, login, test_app};
use crate::users::get_users;

async fn patch_user(app: &TestApp, token: &str, username: &str, body: Value) -> (u16, Value) {
    let r = client()
        .patch(app.url(&format!("/users/{username}")))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

fn code(body: &Value) -> Option<&str> {
    body["error"]["code"].as_str()
}

#[tokio::test]
async fn a_user_edits_their_profile_and_changes_their_password_with_the_current_one() {
    let app = test_app().await;
    create_user(&app.db, "bob", "pw-123456", false).await;
    create_user(&app.db, "alice", "pw-123456", true).await;
    let bob = login(&app, "bob", "pw-123456").await;
    let (status, body) = patch_user(
        &app,
        &bob,
        "bob",
        json!({"display_name": "Robert", "email": "bob@example.com"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["display_name"], "Robert");
    assert_eq!(body["email"], "bob@example.com");
    let (_, body) = patch_user(&app, &bob, "bob", json!({"email": null})).await;
    assert_eq!(body["email"], Value::Null, "null clears: {body}");
    let (status, body) = patch_user(&app, &bob, "bob", json!({"password": "new-pw-12345"})).await;
    assert_eq!(
        (status, code(&body)),
        (401, Some("unauthenticated")),
        "no current_password: {body}"
    );
    let (status, _) = patch_user(
        &app,
        &bob,
        "bob",
        json!({"password": "new-pw-12345", "current_password": "wrong-pw"}),
    )
    .await;
    assert_eq!(status, 401);
    let (status, body) = patch_user(
        &app,
        &bob,
        "bob",
        json!({"password": "new-pw-12345", "current_password": "pw-123456"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(!body.to_string().contains("new-pw-12345"), "{body}");
    login(&app, "bob", "new-pw-12345").await;
    let r = client()
        .post(app.url("/session"))
        .json(&json!({"username": "bob", "password": "pw-123456", "token_name": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401, "the old password is gone");
    assert_eq!(
        get_users(&app, Some(&bob)).await.0,
        403,
        "bob's old token still works but he is no admin"
    );
    for body in [json!({"server_admin": true}), json!({"disabled": false})] {
        let (status, err) = patch_user(&app, &bob, "bob", body.clone()).await;
        assert_eq!(
            (status, code(&err)),
            (403, Some("forbidden")),
            "{body}: {err}"
        );
    }
    let (status, body) = patch_user(&app, &bob, "alice", json!({"display_name": "x"})).await;
    assert_eq!((status, code(&body)), (404, Some("not_found")), "{body}");
    let (status, body) = patch_user(&app, &bob, "bob", json!({"display_name": ""})).await;
    assert_eq!(
        (status, body["error"]["field"].as_str()),
        (422, Some("display_name")),
        "{body}"
    );
}

#[tokio::test]
async fn current_password_guesses_are_rate_limited() {
    let app = test_app().await;
    create_user(&app.db, "bob", "pw-123456", false).await;
    let bob = login(&app, "bob", "pw-123456").await;
    // The login above counted one attempt for `bob`; four wrong guesses fill the window of five.
    for i in 0..4 {
        let (status, body) = patch_user(
            &app,
            &bob,
            "bob",
            json!({"password": "new-pw-12345", "current_password": "wrong-pw"}),
        )
        .await;
        assert_eq!(status, 401, "guess {i}: {body}");
    }
    let r = client()
        .patch(app.url("/users/bob"))
        .bearer_auth(&bob)
        .json(&json!({"password": "new-pw-12345", "current_password": "pw-123456"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 429);
    let retry: u64 = r.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (1..=60).contains(&retry),
        "Retry-After is the rest of the window: {retry}"
    );
    let body: Value = r.json().await.unwrap();
    assert_eq!(code(&body), Some("rate_limited"), "{body}");
    let (status, _) = patch_user(&app, &bob, "bob", json!({"display_name": "Still Bob"})).await;
    assert_eq!(
        status, 200,
        "a patch without a password check is not limited"
    );
}

#[tokio::test]
async fn an_admin_edits_anyone_and_disabling_stops_tokens() {
    let app = test_app().await;
    create_user(&app.db, "bob", "pw-123456", false).await;
    create_user(&app.db, "alice", "pw-123456", true).await;
    let bob = login(&app, "bob", "pw-123456").await;
    let alice = login(&app, "alice", "pw-123456").await;
    let (status, body) = patch_user(&app, &alice, "bob", json!({"disabled": true})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["disabled"], true);
    assert_eq!(
        client()
            .get(app.url("/me"))
            .bearer_auth(&bob)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        401,
        "disabled stops tokens"
    );
    let (_, body) = patch_user(
        &app,
        &alice,
        "bob",
        json!({"disabled": false, "server_admin": true}),
    )
    .await;
    assert_eq!(
        (body["disabled"].as_bool(), body["server_admin"].as_bool()),
        (Some(false), Some(true))
    );
    assert_eq!(
        client()
            .get(app.url("/me"))
            .bearer_auth(&bob)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        200,
        "re-enabled"
    );
    let (status, body) = patch_user(&app, &alice, "bob", json!({"password": "reset-pw-123"})).await;
    assert_eq!(
        status, 200,
        "an admin resets without current_password: {body}"
    );
    login(&app, "bob", "reset-pw-123").await;
    let (status, _) = patch_user(&app, &alice, "alice", json!({"password": "alice-pw-2"})).await;
    assert_eq!(
        status, 401,
        "self password change needs current_password even for an admin"
    );
    let (status, _) = patch_user(
        &app,
        &alice,
        "alice",
        json!({"password": "alice-pw-2", "current_password": "pw-123456"}),
    )
    .await;
    assert_eq!(status, 200);
    let (status, body) = patch_user(&app, &alice, "nobody", json!({"display_name": "x"})).await;
    assert_eq!((status, code(&body)), (404, Some("not_found")), "{body}");
    let (status, body) = patch_user(&app, &alice, "bob", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["username"], "bob");
}

#[tokio::test]
async fn patched_display_name_and_email_are_stored_trimmed() {
    let app = test_app().await;
    create_user(&app.db, "bob", "pw-123456", false).await;
    let bob = login(&app, "bob", "pw-123456").await;
    let body = json!({"display_name": "  Robert  ", "email": " bob@example.com "});
    let (status, body) = patch_user(&app, &bob, "bob", body).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["display_name"], "Robert", "{body}");
    assert_eq!(body["email"], "bob@example.com", "{body}");
}
