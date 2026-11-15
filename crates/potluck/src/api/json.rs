//! JSON request bodies in the error model.

use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, FailedToBufferBody};
use axum::extract::{FromRequest, Request};
use axum::http::header::CONTENT_TYPE;
use serde::de::DeserializeOwned;
use serde_json::error::Category;

use super::error::ApiError;

/// A JSON body in the error model: a wrong or missing `Content-Type`, invalid JSON or the wrong
/// shape are 400 `invalid_request`; a body over the limit is 413 `payload_too_large`. A missing
/// field is named ("missing field `count`"); no other serde text, and never a value, reaches the
/// message. A plain newtype on purpose: handlers and tests destructure it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiJson<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        if !is_json(&req) {
            return Err(ApiError::invalid_request(
                "Content-Type must be application/json",
            ));
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
            ApiError::invalid_request(message)
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
