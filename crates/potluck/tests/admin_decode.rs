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

/// The generated password a command printed, from its `password: ` line or its JSON wrap.
fn printed_password(out: &str) -> String {
    let table = out.lines().find_map(|l| l.strip_prefix("password: "));
    let json = serde_json::from_str::<serde_json::Value>(out).ok();
    let json = json.as_ref().and_then(|v| v["password"].as_str());
    table
        .or(json)
        .unwrap_or_else(|| panic!("a generated password on stdout: {out:?}"))
        .to_owned()
}

#[tokio::test]
async fn a_generated_password_is_printed_before_an_unreadable_answer() {
    let dir = tempfile::tempdir().unwrap();
    let router = Router::new()
        .route(
            "/api/v1/users",
            axum::routing::post(|| async { (axum::http::StatusCode::CREATED, "") }),
        )
        .route(
            "/api/v1/users/{name}",
            axum::routing::patch(|| async { (axum::http::StatusCode::OK, "") }),
        );
    let server = stand_in(dir.path(), router);
    let commands: [(&[&str], u16); 3] = [
        (&["user", "create", "ada"], 201),
        (&["--json", "user", "create", "ada"], 201),
        (&["user", "reset-password", "ada"], 200),
    ];
    for (args, expected) in commands {
        let (result, out) = admin(dir.path(), args, "").await;
        let password = printed_password(&out);
        assert!(
            password.len() == 32,
            "{args:?} printed a generated password: {out:?}"
        );
        match &result {
            Err(e @ AdminError::Malformed { status }) => {
                assert_eq!(*status, expected, "{args:?}");
                assert!(
                    !format!("{e} {e:?}").contains(&password),
                    "{args:?} error holds no password"
                );
            }
            other => panic!("{args:?} is Malformed: {other:?}"),
        }
    }
    server.abort();
}
