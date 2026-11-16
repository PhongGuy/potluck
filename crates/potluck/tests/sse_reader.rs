//! The test-side SSE reader against a hand-rolled stream: blocks, skipped comments, the end.

use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::routing::get;
use futures_util::stream;

use crate::common::{SseReader, ref_event, serve};

async fn events() -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    Sse::new(stream::iter([
        Ok::<_, Infallible>(Event::default().retry(Duration::from_secs(2))),
        Ok(Event::default().comment("keep-alive")),
        Ok(Event::default()
            .event("ref")
            .json_data(serde_json::json!({"ref": "refs/heads/main", "sha": "abc"}))
            .unwrap()),
    ]))
}

#[tokio::test]
async fn reads_blocks_skips_comments_and_sees_the_end() {
    let router = Router::new()
        .route("/events", get(events))
        .route("/nope", get(|| async { (StatusCode::NOT_FOUND, "") }));
    let (addr, _task) = serve(router).await;
    let mut r = SseReader::open(&format!("http://{addr}/events"), "token")
        .await
        .unwrap();
    assert_eq!(r.next_block().await.unwrap(), vec!["retry: 2000"]);
    assert_eq!(
        ref_event(&r.next_block().await.unwrap()),
        ("refs/heads/main".to_owned(), "abc".to_owned()),
        "the comment-only block was skipped"
    );
    assert_eq!(r.next_block().await, None, "the server ended the stream");
    assert_eq!(
        SseReader::open(&format!("http://{addr}/nope"), "token")
            .await
            .err(),
        Some(404)
    );
}

async fn crlf_events() -> impl axum::response::IntoResponse {
    (
        [("content-type", "text/event-stream")],
        "retry: 2000\r\n\r\n: keep-alive\r\n\r\nevent: ref\r\ndata: {\"ref\":\"refs/heads/main\",\"sha\":\"abc\"}\r\n\r\n",
    )
}

async fn unterminated_events() -> impl axum::response::IntoResponse {
    (
        [("content-type", "text/event-stream")],
        "event: ref\ndata: {\"ref\":\"refs/heads/main\",\"sha\":\"abc\"}\n\nevent: ref\ndata: {\"ref\":\"refs/heads/next\",\"sha\":\"def\"}\n",
    )
}

#[tokio::test]
async fn crlf_line_endings_split_blocks_like_lf() {
    let router = Router::new().route("/events", get(crlf_events));
    let (addr, _task) = serve(router).await;
    let mut r = SseReader::open(&format!("http://{addr}/events"), "token")
        .await
        .unwrap();
    assert_eq!(r.next_block().await.unwrap(), vec!["retry: 2000"]);
    assert_eq!(
        ref_event(&r.next_block().await.unwrap()),
        ("refs/heads/main".to_owned(), "abc".to_owned()),
        "the CRLF comment block was skipped and the event read"
    );
    assert_eq!(r.next_block().await, None, "the server ended the stream");
}

#[tokio::test]
#[should_panic(expected = "stream ended mid-event")]
async fn an_unterminated_final_block_is_reported_not_dropped() {
    let router = Router::new().route("/events", get(unterminated_events));
    let (addr, _task) = serve(router).await;
    let mut r = SseReader::open(&format!("http://{addr}/events"), "token")
        .await
        .unwrap();
    assert_eq!(
        ref_event(&r.next_block().await.unwrap()),
        ("refs/heads/main".to_owned(), "abc".to_owned())
    );
    // A client discards an event the stream ends inside, so the reader must not hand it back
    // as one, nor hide it behind a clean end.
    let _ = r.next_block().await;
}
