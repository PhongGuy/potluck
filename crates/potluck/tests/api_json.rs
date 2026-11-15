//! JSON bodies answer the error model and never echo values.

use axum::Router;
use axum::routing::post;
use potluck::api::json::ApiJson;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::common::{client, serve};

#[derive(Deserialize, Serialize)]
struct Body {
    name: String,
    count: u32,
}

async fn echo(ApiJson(body): ApiJson<Body>) -> axum::Json<Body> {
    axum::Json(body)
}

/// The echo route's URL; the server task is detached and ends with the test runtime.
async fn app() -> String {
    let (addr, _task) = serve(Router::new().route("/echo", post(echo))).await;
    format!("http://{addr}/echo")
}

async fn send(url: &str, content_type: Option<&str>, body: String) -> (u16, Value) {
    let mut req = client().post(url).body(body);
    if let Some(ct) = content_type {
        req = req.header("content-type", ct);
    }
    let r = req.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

#[tokio::test]
async fn a_valid_body_is_passed_through_and_unknown_fields_are_ignored() {
    let url = app().await;
    let (status, body) = send(
        &url,
        Some("application/json"),
        r#"{"name": "x", "count": 2, "extra": true}"#.into(),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({"name": "x", "count": 2}));
    let (status, _) = send(
        &url,
        Some("application/json; charset=utf-8"),
        r#"{"name": "x", "count": 2}"#.into(),
    )
    .await;
    assert_eq!(status, 200, "parameters on the media type are fine");
}

#[tokio::test]
async fn a_wrong_or_missing_content_type_is_400() {
    let url = app().await;
    for ct in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
    ] {
        let (status, body) = send(&url, ct, r#"{"name": "x", "count": 2}"#.into()).await;
        assert_eq!(status, 400, "{ct:?}: {body}");
        assert_eq!(body["error"]["code"], "invalid_request");
        assert_eq!(body["error"]["field"], Value::Null);
    }
}

#[tokio::test]
async fn invalid_json_and_the_wrong_shape_are_400_without_echoing_values() {
    let url = app().await;
    let (status, body) = send(&url, Some("application/json"), "{not json".into()).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
    let (status, body) = send(&url, Some("application/json"), r#"{"name": "x"}"#.into()).await;
    assert_eq!(status, 400);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing field `count`"),
        "{body}"
    );
    let (status, body) = send(
        &url,
        Some("application/json"),
        r#"{"name": "plk_secretvalue", "count": "plk_secretvalue"}"#.into(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        !body.to_string().contains("plk_secretvalue"),
        "a value leaked: {body}"
    );
    let (status, body) = send(&url, Some("application/json"), String::new()).await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test]
async fn an_oversized_body_is_413() {
    let url = app().await;
    // axum's default body limit is 2 MiB; this is 3 MiB of valid JSON.
    let big = format!(
        r#"{{"name": "{}", "count": 1}}"#,
        "a".repeat(3 * 1024 * 1024)
    );
    let (status, body) = send(&url, Some("application/json"), big).await;
    assert_eq!(status, 413, "{body}");
    assert_eq!(body["error"]["code"], "payload_too_large");
}
