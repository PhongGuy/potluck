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

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Every status a constructor sets is a valid one, so the fallback never fires.
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut headers = HeaderMap::new();
        if self.code == ErrorCode::Unauthenticated {
            headers.insert(
                WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"potluck\""),
            );
        }
        if let Some(secs) = self.retry_after {
            headers.insert(RETRY_AFTER, HeaderValue::from(secs));
        }
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code.as_str(),
                message: &self.message,
                field: self.field.as_deref(),
            },
        };
        (status, headers, Json(body)).into_response()
    }
}

impl From<DbError> for ApiError {
    // The operator gets the SQLite text in the log; the client gets a fixed message, since
    // that text can name tables and values.
    fn from(err: DbError) -> Self {
        tracing::error!(error = %err, "database error");
        match err {
            DbError::Closed => ApiError::unavailable("database unavailable"),
            DbError::Sqlite(_) | DbError::SchemaTooNew { .. } => {
                ApiError::internal("database error")
            }
        }
    }
}
