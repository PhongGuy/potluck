//! The JSON error model: every non-2xx answer is `{"error": {"code", "message", "field"}}`.

use axum::Json;
use axum::http::header::{RETRY_AFTER, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::db::DbError;

/// Stable error codes of the JSON error model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorCode {
    /// 401: no credentials, or credentials that do not check out.
    Unauthenticated,
    /// 403: authenticated, but the role does not allow this.
    Forbidden,
    /// 404: no such resource or route.
    NotFound,
    /// 409: the request clashes with the current state.
    Conflict,
    /// 400 for a malformed request, 422 for a well-formed one with a bad value.
    InvalidRequest,
    /// 413: the body is over the configured limit.
    PayloadTooLarge,
    /// 429: too many attempts; retry after the given delay.
    RateLimited,
    /// 500: the server failed.
    Internal,
    /// 503: a dependency, such as the database, is down.
    Unavailable,
    /// 400: an OIDC handoff code that is unknown, used, late, or does not match its verifier.
    OidcCodeInvalid,
}

impl ErrorCode {
    /// The wire spelling, snake_case as in the spec (`not_found`, `invalid_request`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Unauthenticated => "unauthenticated",
            ErrorCode::Forbidden => "forbidden",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Conflict => "conflict",
            ErrorCode::InvalidRequest => "invalid_request",
            ErrorCode::PayloadTooLarge => "payload_too_large",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::Internal => "internal",
            ErrorCode::Unavailable => "unavailable",
            ErrorCode::OidcCodeInvalid => "oidc_code_invalid",
        }
    }
}

/// A non-2xx answer: `{"error": {"code", "message", "field"}}` plus the status and, for
/// `unauthenticated` and `rate_limited`, a header.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ApiError {
    status: u16,
    code: ErrorCode,
    message: String,
    field: Option<String>,
    retry_after: Option<u64>,
}

impl ApiError {
    fn new(status: u16, code: ErrorCode, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
            field: None,
            retry_after: None,
        }
    }

    /// 401 `unauthenticated`, with `WWW-Authenticate: Basic realm="potluck"`.
    pub fn unauthenticated(message: impl Into<String>) -> Self {
        Self::new(401, ErrorCode::Unauthenticated, message)
    }

    /// 403 `forbidden`.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(403, ErrorCode::Forbidden, message)
    }

    /// 404 `not_found`.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, ErrorCode::NotFound, message)
    }

    /// 409 `conflict`.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(409, ErrorCode::Conflict, message)
    }

    /// 400 `invalid_request`, for a request that does not parse.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(400, ErrorCode::InvalidRequest, message)
    }

    /// 422 `invalid_request` naming the offending `field`.
    pub fn invalid_value(field: impl Into<String>, message: impl Into<String>) -> Self {
        ApiError {
            field: Some(field.into()),
            ..Self::new(422, ErrorCode::InvalidRequest, message)
        }
    }

    /// 413 `payload_too_large`.
    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::new(413, ErrorCode::PayloadTooLarge, message)
    }

    /// 429 `rate_limited`, with `Retry-After: <retry_after_secs>`.
    pub fn rate_limited(retry_after_secs: u64) -> Self {
        ApiError {
            retry_after: Some(retry_after_secs),
            ..Self::new(429, ErrorCode::RateLimited, "too many requests")
        }
    }

    /// 500 `internal`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, ErrorCode::Internal, message)
    }

    /// 503 `unavailable`.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(503, ErrorCode::Unavailable, message)
    }

    /// 400 `oidc_code_invalid`.
    pub fn oidc_code_invalid(message: impl Into<String>) -> Self {
        Self::new(400, ErrorCode::OidcCodeInvalid, message)
    }

    /// The HTTP status this error answers with.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The stable code in the body.
    pub fn code(&self) -> ErrorCode {
        self.code
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorDetail<'a>,
}

#[derive(Serialize)]
struct ErrorDetail<'a> {
    code: &'static str,
    message: &'a str,
    field: Option<&'a str>,
}

/// An `ApiError` on its way out as an HTTP response.
pub(crate) struct ErrorResponse(ApiError);

/// What a handler returns: `?` turns any error `ApiError` converts from into the response.
pub(crate) type ApiResult<T> = Result<T, ErrorResponse>;

impl<E> From<E> for ErrorResponse
where
    ApiError: From<E>,
{
    fn from(err: E) -> Self {
        ErrorResponse(ApiError::from(err))
    }
}

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> Response {
        let ErrorResponse(err) = self;
        // Every status a constructor sets is a valid one, so the fallback never fires.
        let status = StatusCode::from_u16(err.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut headers = HeaderMap::new();
        if err.code == ErrorCode::Unauthenticated {
            headers.insert(
                WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"potluck\""),
            );
        }
        if let Some(secs) = err.retry_after {
            headers.insert(RETRY_AFTER, HeaderValue::from(secs));
        }
        let body = ErrorBody {
            error: ErrorDetail {
                code: err.code.as_str(),
                message: &err.message,
                field: err.field.as_deref(),
            },
        };
        (status, headers, Json(body)).into_response()
    }
}

impl From<DbError> for ApiError {
    // A conflict is the client's mistake and its text is client-safe, so it passes through
    // unlogged. For the rest the operator gets the SQLite text in the log and the client a
    // fixed message, since that text can name tables and values.
    fn from(err: DbError) -> Self {
        match err {
            DbError::Conflict { what } => ApiError::conflict(what),
            // Closed means the server is shutting down, which is expected rather than a fault.
            err @ DbError::Closed => {
                tracing::warn!(error = %err, "database closed, shutting down");
                ApiError::unavailable("database unavailable")
            }
            err @ (DbError::Sqlite(_) | DbError::SchemaTooNew { .. }) => {
                tracing::error!(error = %err, "database error");
                ApiError::internal("database error")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    async fn render(err: ApiError) -> (u16, axum::http::HeaderMap, Value) {
        let response = ErrorResponse::from(err).into_response();
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
            (ApiError::oidc_code_invalid("x"), 400, "oidc_code_invalid"),
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
        let (status, _, body) =
            render(ApiError::invalid_value("expires_at", "not a timestamp")).await;
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
}
