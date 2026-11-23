//! `IdpHttp`: no redirects, a body cap and a total timeout, and errors that carry no URL.

use std::time::{Duration, Instant};

use axum::Router;
use axum::http::{StatusCode, header};
use axum::routing::get;
use potluck::oidc::{IdpHttp, IdpHttpError};

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

fn routes() -> Router {
    Router::new()
        .route("/ok", get(|| async { "hello" }))
        .route("/missing", get(|| async { (StatusCode::NOT_FOUND, "no") }))
        .route(
            "/redirect",
            get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/ok")]) }),
        )
        .route("/big", get(|| async { vec![b'x'; 2 * 1024 * 1024] }))
        .route(
            "/hang",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "late"
            }),
        )
}

fn http() -> IdpHttp {
    IdpHttp::with_limits(
        false,
        Duration::from_secs(1),
        Duration::from_millis(500),
        1024 * 1024,
    )
    .unwrap()
}

fn assert_no_url(err: IdpHttpError) {
    let text = err.to_string();
    assert!(!text.contains("127.0.0.1"), "{text}");
    assert!(!text.contains("http"), "{text}");
}

#[tokio::test]
async fn returns_the_status_and_the_body() {
    let base = serve(routes()).await;
    assert_eq!(
        http().get(&format!("{base}/ok")).await.unwrap(),
        (200, b"hello".to_vec())
    );
    assert_eq!(http().get(&format!("{base}/missing")).await.unwrap().0, 404);
}

#[tokio::test]
async fn never_follows_a_redirect() {
    let base = serve(routes()).await;
    let err = http().get(&format!("{base}/redirect")).await.unwrap_err();
    assert_eq!(err, IdpHttpError::Redirect);
    assert_no_url(err);
}

#[tokio::test]
async fn refuses_a_body_over_the_cap() {
    let base = serve(routes()).await;
    let err = http().get(&format!("{base}/big")).await.unwrap_err();
    assert_eq!(err, IdpHttpError::TooLarge);
    assert_no_url(err);
}

#[tokio::test]
async fn gives_up_at_the_total_timeout() {
    let base = serve(routes()).await;
    let started = Instant::now();
    let err = http().get(&format!("{base}/hang")).await.unwrap_err();
    assert_eq!(err, IdpHttpError::Timeout);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_no_url(err);
}

#[tokio::test]
async fn an_unreachable_host_is_a_transport_error() {
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let err = http()
        .get(&format!("http://127.0.0.1:{port}/ok"))
        .await
        .unwrap_err();
    assert_eq!(err, IdpHttpError::Transport);
    assert_no_url(err);
}

#[tokio::test]
async fn https_only_refuses_plain_http() {
    let base = serve(routes()).await;
    let strict = IdpHttp::new(true).unwrap();
    assert_eq!(
        strict.get(&format!("{base}/ok")).await.unwrap_err(),
        IdpHttpError::Transport
    );
}

#[test]
fn the_default_client_builds() {
    IdpHttp::new(false).unwrap();
}
