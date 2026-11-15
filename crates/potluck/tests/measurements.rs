//! Measurement 1 of the spec, pinned so a dependency bump that changes it fails here.
//! The second half (serving on a `UnixListener`) is exercised by `serve.rs`.

#[test]
fn axum_sse_keep_alive_defaults_to_15_seconds() {
    let rendered = format!("{:?}", axum::response::sse::KeepAlive::default());
    assert!(
        rendered.contains("max_interval: 15s"),
        "axum 0.8.9 default changed: {rendered}"
    );
}
