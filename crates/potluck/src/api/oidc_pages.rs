//! The fixed HTML pages the browser sees during an OIDC sign-in. They echo no request data, and
//! every one leaves through [`page`] so the security headers are set in one place.

use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY, RETRY_AFTER,
};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

/// 422: the app's start link broke a parameter rule.
pub(super) const BAD_START: &str = "<!doctype html><meta charset=\"utf-8\"><title>Sign-in failed</title>\
<h1>Sign-in could not start</h1><p>The sign-in link from your app is not valid. \
Close this tab and start the sign-in again from the app.</p>";

/// 400: the callback names no sign-in in flight (never issued, already used, or long gone).
pub(super) const START_AGAIN: &str = "<!doctype html><meta charset=\"utf-8\"><title>Sign-in expired</title>\
<h1>This sign-in is no longer valid</h1><p>It was already used, or it is too old. \
Close this tab and start the sign-in again from the app.</p>";

/// 429: too many starts from this address.
pub(super) const TOO_MANY: &str = "<!doctype html><meta charset=\"utf-8\"><title>Too many sign-ins</title>\
<h1>Too many sign-in attempts</h1><p>Wait a minute, then start the sign-in again from your app.</p>";

/// 503: the identity provider cannot be reached.
pub(super) const IDP_UNAVAILABLE: &str = "<!doctype html><meta charset=\"utf-8\"><title>Sign-in unavailable</title>\
<h1>Sign-in is unavailable</h1><p>The identity provider cannot be reached right now. \
Try again later, or ask your administrator.</p>";

/// `body` as an HTML answer with `status`, no scripts, styles, caching or referrer.
pub(super) fn page(status: StatusCode, body: &'static str) -> Response {
    (
        status,
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (CONTENT_SECURITY_POLICY, "default-src 'none'"),
            (CACHE_CONTROL, "no-store"),
            (REFERRER_POLICY, "no-referrer"),
        ],
        body,
    )
        .into_response()
}

/// The 429 page with `Retry-After`.
pub(super) fn too_many(retry_after_secs: u64) -> Response {
    let mut response = page(StatusCode::TOO_MANY_REQUESTS, TOO_MANY);
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from(retry_after_secs));
    response
}
