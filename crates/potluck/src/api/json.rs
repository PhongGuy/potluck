//! JSON request bodies in the error model.

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, FailedToBufferBody};
use axum::extract::{FromRequest, Request};
use axum::http::header::CONTENT_TYPE;
use serde::de::DeserializeOwned;
use serde_json::error::Category;

use super::error::{ApiError, ErrorResponse};

/// A JSON body in the error model: a wrong or missing `Content-Type`, invalid JSON or the wrong
/// shape are 400 `invalid_request`; a body over the limit is 413 `payload_too_large`. A missing
/// field is named ("missing field `count`"); no other serde text, and never a value, reaches the
/// message. A plain newtype on purpose: handlers and tests destructure it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ApiJson<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for ApiJson<T> {
    type Rejection = ErrorResponse;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        if !is_json(&req) {
            return Err(ApiError::invalid_request("Content-Type must be application/json").into());
        }
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|rejection| match rejection {
                BytesRejection::FailedToBufferBody(FailedToBufferBody::LengthLimitError(_)) => {
                    ApiError::payload_too_large("request body is over the size limit")
                }
                _ => ApiError::invalid_request("request body could not be read"),
            })?;
        // Serde's text can quote the offending value, so only the missing-field message, which
        // names a field of our own type, is passed through.
        serde_json::from_slice(&bytes).map(ApiJson).map_err(|e| {
            let message = match e.classify() {
                Category::Data if e.to_string().starts_with("missing field") => e.to_string(),
                Category::Data => "request body does not have the expected shape".to_owned(),
                Category::Syntax | Category::Eof | Category::Io => {
                    "request body is not valid JSON".to_owned()
                }
            };
            ApiError::invalid_request(message).into()
        })
    }
}

/// Whether the media type, ignoring parameters and ASCII case, is `application/json`.
fn is_json(req: &Request) -> bool {
    req.headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::routing::post;
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};

    use super::ApiJson;
    use crate::api::test_support::serve;

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
        let mut req = reqwest::Client::new().post(url).body(body);
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
}
