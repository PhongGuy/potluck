//! The JSON error model: body shape, status per code, the two headers, and no SQLite leaks.

use axum::response::IntoResponse;
use potluck::api::error::{ApiError, ErrorCode};
use potluck::db::DbError;
use serde_json::{Value, json};

async fn render(err: ApiError) -> (u16, axum::http::HeaderMap, Value) {
    let response = err.into_response();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .unwrap();
    (status, headers, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn bodies_follow_the_error_model() {
    let (status, headers, body) = render(ApiError::not_found("no such workspace")).await;
    assert_eq!(status, 404);
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(
        body,
        json!({"error": {"code": "not_found", "message": "no such workspace", "field": null}})
    );
}

#[tokio::test]
async fn every_constructor_maps_to_its_status_and_code() {
    let cases: Vec<(ApiError, u16, &str)> = vec![
        (ApiError::unauthenticated("x"), 401, "unauthenticated"),
        (ApiError::forbidden("x"), 403, "forbidden"),
        (ApiError::not_found("x"), 404, "not_found"),
        (ApiError::conflict("x"), 409, "conflict"),
        (ApiError::invalid_request("x"), 400, "invalid_request"),
        (ApiError::invalid_value("name", "x"), 422, "invalid_request"),
        (ApiError::payload_too_large("x"), 413, "payload_too_large"),
        (ApiError::rate_limited(30), 429, "rate_limited"),
        (ApiError::internal("x"), 500, "internal"),
        (ApiError::unavailable("x"), 503, "unavailable"),
    ];
    for (err, want_status, want_code) in cases {
        assert_eq!(err.status(), want_status, "{want_code}");
        assert_eq!(err.code().as_str(), want_code);
        let (status, _, body) = render(err).await;
        assert_eq!(status, want_status, "{want_code}");
        assert_eq!(body["error"]["code"], want_code);
    }
}

#[tokio::test]
async fn unauthenticated_sets_www_authenticate() {
    let (_, headers, _) = render(ApiError::unauthenticated("bad token")).await;
    assert_eq!(headers["www-authenticate"], "Basic realm=\"potluck\"");
}

#[tokio::test]
async fn rate_limited_sets_retry_after() {
    let (_, headers, body) = render(ApiError::rate_limited(30)).await;
    assert_eq!(headers["retry-after"], "30");
    assert_eq!(body["error"]["code"], "rate_limited");
}

#[tokio::test]
async fn invalid_value_is_422_with_the_field() {
    let (status, _, body) = render(ApiError::invalid_value("expires_at", "not a timestamp")).await;
    assert_eq!(status, 422);
    assert_eq!(
        body["error"],
        json!({"code": "invalid_request", "message": "not a timestamp", "field": "expires_at"})
    );
}

#[tokio::test]
async fn database_errors_never_leak_sqlite_text() {
    let (status, _, body) = render(ApiError::from(DbError::Sqlite(
        "no such table: secrets".into(),
    )))
    .await;
    assert_eq!(status, 500);
    assert_eq!(body["error"]["code"], "internal");
    assert!(
        !body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("secrets"),
        "{body}"
    );
    let (status, _, body) = render(ApiError::from(DbError::Closed)).await;
    assert_eq!(status, 503);
    assert_eq!(body["error"]["code"], "unavailable");
    assert_eq!(ErrorCode::Unavailable.as_str(), "unavailable");
}
