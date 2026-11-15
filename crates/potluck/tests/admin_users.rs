//! `potluck admin user ...` in process against a started server.

use potluck::admin::AdminError;
use potluck::admin::client::{AdminClient, Verb};
use potluck::admin::output::table;
use potluck::server::Running;
use serde_json::{Value, json};

use crate::common::{admin, admin_server, columns};

/// `POST /users` straight over the socket, so these tests do not depend on `user create`.
async fn post_user(running: &Running, name: &str) {
    let client = AdminClient::connect(running.admin_socket()).await.unwrap();
    let body = json!({"username": name, "display_name": name, "password": "pw-123456"});
    let r = client
        .send(Verb::Post, "/api/v1/users", Some(&body.to_string()))
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text());
}

#[tokio::test]
async fn user_list_prints_a_table_or_the_json_array() {
    let (dir, running) = admin_server().await;
    for name in ["ann", "bob"] {
        post_user(&running, name).await;
    }
    let (result, out) = admin(dir.path(), &["user", "list"], "").await;
    result.unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        columns(lines[0]),
        [
            "USERNAME",
            "DISPLAY NAME",
            "EMAIL",
            "ADMIN",
            "DISABLED",
            "CREATED"
        ],
        "{out}"
    );
    assert_eq!(lines.len(), 3, "a header and two rows: {out}");
    assert!(
        lines[1].starts_with("ann ") && lines[2].starts_with("bob "),
        "{out}"
    );
    assert_eq!(
        columns(lines[1])[2..5],
        ["-", "no", "no"],
        "null email, flags as words: {out}"
    );
    assert!(
        !out.ends_with(' ') && !out.contains(" \n"),
        "no trailing spaces: {out:?}"
    );
    let (result, out) = admin(dir.path(), &["user", "list", "--json"], "").await;
    result.unwrap();
    let v: Value = serde_json::from_str(&out).expect("json stdout");
    assert_eq!(v.as_array().map(Vec::len), Some(2));
    assert_eq!(v[0]["username"], "ann");
    running.shutdown().await;
}

#[tokio::test]
async fn the_global_flags_go_before_or_after_the_verb() {
    let (dir, running) = admin_server().await;
    let (result, before) = admin(dir.path(), &["--json", "user", "list"], "").await;
    result.unwrap();
    let (result, after) = admin(dir.path(), &["user", "list", "--json"], "").await;
    result.unwrap();
    assert_eq!(before, after);
    assert_eq!(before.trim(), "[]");
    running.shutdown().await;
}

#[tokio::test]
async fn without_a_server_the_error_names_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let (result, out) = admin(dir.path(), &["user", "list"], "").await;
    let err = result.unwrap_err();
    assert!(matches!(err, AdminError::Unreachable { .. }), "got {err:?}");
    assert!(err.to_string().contains("admin.sock"), "{err}");
    assert_eq!(out, "");
}

#[test]
fn table_pads_columns_and_leaves_no_trailing_spaces() {
    let rows = vec![
        vec!["ann".to_owned(), "-".to_owned()],
        vec!["bobby".to_owned(), "x".to_owned()],
    ];
    assert_eq!(
        table(&["USER", "EMAIL"], &rows),
        "USER   EMAIL\nann    -\nbobby  x\n"
    );
}
