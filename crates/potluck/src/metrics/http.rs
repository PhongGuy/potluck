//! Times every API response body and counts rejected credentials.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use http_body::{Frame, SizeHint};

use super::{AuthScheme, Metrics, method_label};

/// The change-notice stream lives for hours, so its duration would only skew the histogram.
const EVENTS_ROUTE: &str = "/api/v1/workspaces/{team}/{ws}/events";

/// Sign-in routes answer 401 for wrong passwords; the sign-in family counts those instead.
const SIGN_IN_ROUTES: [&str; 2] = ["/api/v1/session", "/api/v1/oidc/redeem"];

/// Records each response under its method, route template and status once its body is
/// finished or abandoned, and counts a 401 to a request that carried credentials by scheme.
/// The events stream passes through unrecorded.
pub(crate) async fn layer(
    State(metrics): State<Arc<Metrics>>,
    req: Request,
    next: Next,
) -> Response {
    // Everything read from the request is taken now, because `next` consumes it.
    let method = method_label(req.method());
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str)
        .to_owned();
    if route == EVENTS_ROUTE {
        return next.run(req).await;
    }
    let scheme = req.headers().get(header::AUTHORIZATION).map(scheme_of);
    let start = Instant::now();
    let response = next.run(req).await;

    let status = response.status();
    if status == StatusCode::UNAUTHORIZED
        && let Some(scheme) = scheme
        && !SIGN_IN_ROUTES.contains(&route.as_str())
    {
        metrics.auth_failure(scheme);
    }
    let code = status.as_u16();
    response.map(|inner| {
        Body::new(Timed {
            inner,
            done: Some(Box::new(move || {
                metrics.observe_http(method, &route, code, start.elapsed().as_secs_f64());
            })),
        })
    })
}

/// The header's first word, case-insensitive; never the credential itself.
fn scheme_of(value: &HeaderValue) -> AuthScheme {
    let word = value
        .as_bytes()
        .split(u8::is_ascii_whitespace)
        .next()
        .unwrap_or_default();
    if word.eq_ignore_ascii_case(b"basic") {
        AuthScheme::Basic
    } else if word.eq_ignore_ascii_case(b"bearer") {
        AuthScheme::Bearer
    } else {
        AuthScheme::Other
    }
}

/// A response body that reports its lifetime once, when it is dropped: hyper drops a body
/// after its last frame and also when the client goes away mid-stream. Size and end-of-stream
/// are forwarded so a fixed-length reply keeps its `Content-Length`.
struct Timed {
    inner: Body,
    done: Option<Box<dyn FnOnce() + Send>>,
}

impl http_body::Body for Timed {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        http_body::Body::is_end_stream(&self.inner)
    }

    fn size_hint(&self) -> SizeHint {
        http_body::Body::size_hint(&self.inner)
    }
}

impl Drop for Timed {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            done();
        }
    }
}
#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    use super::layer;
    use crate::metrics::{Metrics, Snapshot, sample};

    fn app(metrics: &Arc<Metrics>, inner: Router) -> Router {
        inner.fallback(|| async { StatusCode::NOT_FOUND }).layer(
            axum::middleware::from_fn_with_state(Arc::clone(metrics), layer),
        )
    }

    async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        auth: Option<&str>,
    ) -> axum::response::Response {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        app.clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn count(m: &Metrics, method: &str, route: &str, status: u16) -> Option<f64> {
        let series = format!(
            "http_server_request_duration_seconds_count{{http_request_method=\"{method}\",http_route=\"{route}\",http_response_status_code=\"{status}\"}}"
        );
        sample(&m.render(&Snapshot::default()), &series)
    }

    #[tokio::test]
    async fn a_matched_route_records_its_template_after_the_body_is_read() {
        let m = Metrics::new("0.1.0");
        let app = app(
            &m,
            Router::new().route("/teams/{team}", get(|| async { "hello" })),
        );
        let res = send(&app, "GET", "/teams/core", None).await;
        assert_eq!(
            axum::body::HttpBody::size_hint(res.body()).exact(),
            Some(5),
            "the wrapper keeps the exact size, so hyper still sends Content-Length"
        );
        assert_eq!(
            count(&m, "GET", "/teams/{team}", 200),
            None,
            "not before the body ends"
        );
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"hello");
        assert_eq!(count(&m, "GET", "/teams/{team}", 200), Some(1.0));
        let text = m.render(&Snapshot::default());
        assert!(!text.contains("core"), "no slug in any label: {text}");
    }

    #[tokio::test]
    async fn unknown_paths_are_unmatched_and_odd_methods_are_other() {
        let m = Metrics::new("0.1.0");
        let app = app(&m, Router::new());
        drop(send(&app, "GET", "/no/such/thing", None).await);
        drop(send(&app, "PROPFIND", "/x", None).await);
        assert_eq!(count(&m, "GET", "unmatched", 404), Some(1.0));
        assert_eq!(count(&m, "_OTHER", "unmatched", 404), Some(1.0));
    }

    #[tokio::test]
    async fn a_streamed_body_is_timed_to_its_end_or_its_drop() {
        let m = Metrics::new("0.1.0");
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(1);
        let rx = Arc::new(std::sync::Mutex::new(Some(rx)));
        let app = app(
            &m,
            Router::new().route(
                "/stream",
                get(move || {
                    let rx = rx.lock().unwrap().take().unwrap();
                    async move { Body::from_stream(tokio_stream_from(rx)) }
                }),
            ),
        );
        let res = send(&app, "GET", "/stream", None).await;
        tx.send(Ok(axum::body::Bytes::from_static(b"a")))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(count(&m, "GET", "/stream", 200), None, "still streaming");
        drop(res);
        assert_eq!(
            count(&m, "GET", "/stream", 200),
            Some(1.0),
            "observed once at drop"
        );
        drop(tx);
        assert_eq!(count(&m, "GET", "/stream", 200), Some(1.0), "never twice");
    }

    /// A receiver as a stream, without tokio-stream.
    fn tokio_stream_from<T: Send + 'static>(
        mut rx: tokio::sync::mpsc::Receiver<T>,
    ) -> impl futures_util::Stream<Item = T> + Send {
        futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
    }

    #[tokio::test]
    async fn the_events_route_is_not_recorded() {
        let m = Metrics::new("0.1.0");
        let app = app(
            &m,
            Router::new().route(
                "/api/v1/workspaces/{team}/{ws}/events",
                get(|| async { "x" }),
            ),
        );
        let res = send(&app, "GET", "/api/v1/workspaces/core/api/events", None).await;
        let _ = axum::body::to_bytes(res.into_body(), 64).await.unwrap();
        let text = m.render(&Snapshot::default());
        assert!(!text.contains("events"), "{text}");
    }

    #[tokio::test]
    async fn a_401_with_authorization_counts_its_scheme_except_on_sign_in_routes() {
        let m = Metrics::new("0.1.0");
        let denied = || async { StatusCode::UNAUTHORIZED };
        let app = app(
            &m,
            Router::new()
                .route("/api/v1/me", get(denied))
                .route("/api/v1/session", get(denied))
                .route("/api/v1/oidc/redeem", get(denied)),
        );
        drop(send(&app, "GET", "/api/v1/me", Some("Bearer plk_nope")).await);
        drop(send(&app, "GET", "/api/v1/me", Some("basic Zm9vOmJhcg==")).await);
        drop(send(&app, "GET", "/api/v1/me", Some("Token abc")).await);
        drop(send(&app, "GET", "/api/v1/me", None).await);
        drop(send(&app, "GET", "/api/v1/session", Some("Bearer plk_nope")).await);
        drop(send(&app, "GET", "/api/v1/oidc/redeem", Some("Bearer plk_nope")).await);
        let text = m.render(&Snapshot::default());
        assert_eq!(
            sample(&text, "potluck_auth_failures_total{scheme=\"bearer\"}"),
            Some(1.0),
            "{text}"
        );
        assert_eq!(
            sample(&text, "potluck_auth_failures_total{scheme=\"basic\"}"),
            Some(1.0),
            "{text}"
        );
        assert_eq!(
            sample(&text, "potluck_auth_failures_total{scheme=\"other\"}"),
            Some(1.0),
            "{text}"
        );
        assert!(!text.contains("plk_nope"), "{text}");
    }
}
