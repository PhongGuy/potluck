//! The admin CLI on a 2xx answer whose body it cannot read: an error, never an empty table.

use axum::Router;
use axum::routing::get;
use potluck::admin::AdminError;
use serde_json::json;

use crate::common::admin;

/// Serves `router` where `admin --data <dir>` looks for the server; abort the handle when done.
fn stand_in(dir: &std::path::Path, router: Router) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::UnixListener::bind(dir.join("admin.sock")).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() })
}

/// Runs each command against the stand-in and asserts a 200 Malformed with nothing printed.
async fn assert_malformed(dir: &std::path::Path, commands: &[&[&str]]) {
    for args in commands {
        let (result, out) = admin(dir, args, "").await;
        assert!(
            matches!(result, Err(AdminError::Malformed { status: 200, .. })),
            "{args:?} is Malformed: {result:?}"
        );
        assert_eq!(out, "", "{args:?} prints nothing");
    }
}

#[tokio::test]
async fn an_empty_or_non_json_2xx_body_is_malformed() {
    let dir = tempfile::tempdir().unwrap();
    let router = Router::new()
        .route("/api/v1/teams", get(|| async { "" }))
        .route("/api/v1/users", get(|| async { "not json" }));
    let server = stand_in(dir.path(), router);
    assert_malformed(
        dir.path(),
        &[
            &["team", "list"],
            &["--json", "team", "list"],
            &["user", "list"],
        ],
    )
    .await;
    server.abort();
}

#[tokio::test]
async fn a_2xx_body_of_the_wrong_json_shape_is_malformed() {
    let dir = tempfile::tempdir().unwrap();
    let router = Router::new()
        .route(
            "/api/v1/workspaces",
            get(|| async { axum::Json(json!({})) }),
        )
        .route(
            "/api/v1/workspaces/core/api",
            get(|| async { axum::Json(json!([])) }),
        );
    let server = stand_in(dir.path(), router);
    assert_malformed(
        dir.path(),
        &[
            &["workspace", "list"],
            &["--json", "workspace", "list"],
            &["workspace", "show", "core/api"],
        ],
    )
    .await;
    server.abort();
}
