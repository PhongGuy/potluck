//! The length checks on names, display names and token names: exact 422 bytes and the limits.

use reqwest::Method;
use serde_json::{Value, json};

use crate::common::{TestApp, api_admin, api_team, api_user, client, create_user, test_app};

/// One request whose body comes back as raw text, so a test can pin it byte for byte.
async fn raw(
    app: &TestApp,
    method: Method,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (u16, String) {
    let mut req = client().request(method, app.url(path)).json(&body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let r = req.send().await.expect("request");
    (r.status().as_u16(), r.text().await.expect("body"))
}

fn refusal(field: &str, max: usize) -> String {
    format!(
        r#"{{"error":{{"code":"invalid_request","message":"{field} must be 1 to {max} characters","field":"{field}"}}}}"#
    )
}

fn workspace(name: &str) -> Value {
    json!({"slug": "api", "name": name, "kind": "apinata", "default_branch": "main"})
}

#[tokio::test]
async fn names_and_display_names_out_of_range_answer_the_same_422_bytes() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_team(&app.url(""), &root, "core").await;
    api_user(&app.db, "bob").await;
    for bad in ["   ".to_owned(), "n".repeat(201)] {
        let cases = [
            (
                Method::POST,
                "/teams",
                json!({"slug": "ok", "name": bad}),
                "name",
            ),
            (
                Method::POST,
                "/teams/core/workspaces",
                workspace(&bad),
                "name",
            ),
            (
                Method::POST,
                "/users",
                json!({"username": "carol", "display_name": bad, "password": "carol-pw-1"}),
                "display_name",
            ),
            (
                Method::PATCH,
                "/users/bob",
                json!({"display_name": bad}),
                "display_name",
            ),
        ];
        for (method, path, body, field) in cases {
            let (status, text) = raw(&app, method, path, Some(&root), body).await;
            assert_eq!(
                (status, text.as_str()),
                (422, refusal(field, 200).as_str()),
                "{path}"
            );
        }
    }
}

#[tokio::test]
async fn two_hundred_characters_of_any_width_are_a_valid_name() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_team(&app.url(""), &root, "core").await;
    api_user(&app.db, "bob").await;
    // 200 two-byte characters, padded: the limit counts characters after trimming, not bytes.
    let wide = "\u{e9}".repeat(200);
    let padded = format!("  {wide}  ");
    let cases = [
        (
            Method::POST,
            "/teams",
            json!({"slug": "ok", "name": padded}),
            "name",
        ),
        (
            Method::POST,
            "/teams/core/workspaces",
            workspace(&padded),
            "name",
        ),
        (
            Method::POST,
            "/users",
            json!({"username": "carol", "display_name": padded, "password": "carol-pw-1"}),
            "display_name",
        ),
        (
            Method::PATCH,
            "/users/bob",
            json!({"display_name": padded}),
            "display_name",
        ),
    ];
    for (method, path, body, field) in cases {
        let (status, text) = raw(&app, method, path, Some(&root), body).await;
        assert!(status == 200 || status == 201, "{path}: {status} {text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v[field], wide.as_str(), "{path} stores the trimmed value");
    }
}

#[tokio::test]
async fn token_names_out_of_range_answer_the_same_422_bytes() {
    let app = test_app().await;
    let (_, bob) = api_user(&app.db, "bob").await;
    for bad in ["   ".to_owned(), "t".repeat(101)] {
        let (status, text) = raw(
            &app,
            Method::POST,
            "/tokens",
            Some(&bob),
            json!({"name": bad}),
        )
        .await;
        assert_eq!(
            (status, text.as_str()),
            (422, refusal("name", 100).as_str())
        );
        let (status, text) = raw(
            &app,
            Method::POST,
            "/session",
            None,
            json!({"username": "bob", "password": "pw-123456", "token_name": bad}),
        )
        .await;
        assert_eq!(
            (status, text.as_str()),
            (422, refusal("token_name", 100).as_str())
        );
    }
}

#[tokio::test]
async fn a_hundred_characters_of_any_width_are_a_valid_token_name() {
    let app = test_app().await;
    create_user(&app.db, "carol", "pw-123456", false).await;
    let (_, bob) = api_user(&app.db, "bob").await;
    let wide = "\u{e9}".repeat(100);
    let padded = format!("  {wide}  ");
    let (status, text) = raw(
        &app,
        Method::POST,
        "/tokens",
        Some(&bob),
        json!({"name": padded}),
    )
    .await;
    assert_eq!(status, 201, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["token_meta"]["name"], wide.as_str());
    let (status, text) = raw(
        &app,
        Method::POST,
        "/session",
        None,
        json!({"username": "carol", "password": "pw-123456", "token_name": padded}),
    )
    .await;
    assert_eq!(status, 201, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["token_meta"]["name"], wide.as_str());
}
