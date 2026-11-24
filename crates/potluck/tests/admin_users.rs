//! `potluck admin user ...` in process against a started server.

use potluck::admin::AdminError;
use potluck::admin::client::{AdminClient, Verb};
use potluck::admin::output::table;
use potluck::server::Running;
use serde_json::{Value, json};

use crate::common::{admin, admin_server, columns, session};

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

#[test]
fn table_escapes_control_and_format_characters_in_every_cell() {
    let rows = vec![
        vec!["x\u{1b}[2Jy".to_owned(), "1".to_owned()],
        vec!["a\u{202e}b\u{200b}".to_owned(), "2".to_owned()],
        vec!["c\u{9b}d\u{feff}".to_owned(), "3".to_owned()],
        vec!["Søren 中文".to_owned(), "4".to_owned()],
    ];
    let out = table(&["A", "B"], &rows);
    for raw in ['\u{1b}', '\u{202e}', '\u{200b}', '\u{9b}', '\u{feff}'] {
        assert!(!out.contains(raw), "raw {raw:?} reached the table: {out:?}");
    }
    assert_eq!(
        out,
        "A                   B\n\
         x\\u{1b}[2Jy         1\n\
         a\\u{202e}b\\u{200b}  2\n\
         c\\u{9b}d\\u{feff}    3\n\
         Søren 中文            4\n",
        "escaped, padded by escaped width, printable text unchanged"
    );
}

#[test]
fn table_escapes_a_literal_backslash() {
    let literal = table(&["NAME"], &[vec!["\\u{1b}".to_owned()]]);
    let control = table(&["NAME"], &[vec!["\u{1b}".to_owned()]]);
    assert_eq!(literal, "NAME\n\\\\u{1b}\n");
    assert_eq!(control, "NAME\n\\u{1b}\n");
    assert_ne!(literal, control);
}

#[tokio::test]
async fn user_list_escapes_control_and_format_characters_in_names() {
    let (dir, running) = admin_server().await;
    let client = AdminClient::connect(running.admin_socket()).await.unwrap();
    let body = json!({
        "username": "eve",
        "display_name": "Eve\u{1b}[31m\u{202e}gnp.exe",
        "password": "pw-123456",
    });
    let r = client
        .send(Verb::Post, "/api/v1/users", Some(&body.to_string()))
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text());
    let (result, out) = admin(dir.path(), &["user", "list"], "").await;
    result.unwrap();
    assert!(
        !out.contains('\u{1b}') && !out.contains('\u{202e}'),
        "no raw control or format character reaches the terminal: {out:?}"
    );
    assert!(out.contains("Eve\\u{1b}[31m\\u{202e}gnp.exe"), "{out}");
    running.shutdown().await;
}

#[tokio::test]
async fn user_list_json_escapes_control_and_format_characters_and_keeps_the_value() {
    let (dir, running) = admin_server().await;
    let client = AdminClient::connect(running.admin_socket()).await.unwrap();
    let name = "a\u{202e}b\u{200b}c\u{9b}\u{7f}\u{2028}\u{e0041}é";
    let body = json!({"username": "eve", "display_name": name, "password": "pw-123456"});
    let r = client
        .send(Verb::Post, "/api/v1/users", Some(&body.to_string()))
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text());
    let (result, out) = admin(dir.path(), &["user", "list", "--json"], "").await;
    result.unwrap();
    for raw in [
        '\u{202e}',
        '\u{200b}',
        '\u{9b}',
        '\u{7f}',
        '\u{2028}',
        '\u{e0041}',
    ] {
        assert!(!out.contains(raw), "raw {raw:?} reached stdout: {out:?}");
    }
    assert!(out.contains('é'), "printable text stays raw: {out:?}");
    assert!(out.contains("\\u202e"), "{out}");
    assert!(
        out.contains("\\udb40\\udc41"),
        "a surrogate pair above the BMP: {out}"
    );
    let v: Value = serde_json::from_str(&out).expect("json stdout");
    assert_eq!(v[0]["display_name"], name, "the decoded value is unchanged");
    running.shutdown().await;
}

fn password_line(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("password: "))
        .map(str::to_owned)
}

#[tokio::test]
async fn user_create_prints_the_row_and_a_generated_password_once() {
    let (dir, running) = admin_server().await;
    let (result, out) = admin(
        dir.path(),
        &["user", "create", "alice", "--server-admin"],
        "",
    )
    .await;
    result.unwrap();
    let password = password_line(&out).expect("a password line");
    assert_eq!(
        password.len(),
        32,
        "24 random bytes as base64url: {password}"
    );
    assert_eq!(out.matches(&password).count(), 1, "printed once: {out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(columns(lines[0])[0], "USERNAME", "{out}");
    assert_eq!(
        columns(lines[1])[..4],
        ["alice", "alice", "-", "yes"],
        "{out}"
    );
    session(running.http_addr(), "alice", &password)
        .await
        .expect("the printed password is the stored one");
    running.shutdown().await;
}

#[tokio::test]
async fn user_create_takes_the_password_from_stdin_and_never_prints_it() {
    let (dir, running) = admin_server().await;
    let (result, out) = admin(
        dir.path(),
        &["user", "create", "bob", "--password-stdin"],
        " spaced pass \r\n",
    )
    .await;
    result.unwrap();
    assert!(
        !out.contains("password") && !out.contains("spaced"),
        "{out}"
    );
    // One trailing newline with its CR is stripped; edge spaces stay.
    session(running.http_addr(), "bob", " spaced pass ")
        .await
        .expect("sign in");
    running.shutdown().await;
}

#[tokio::test]
async fn user_create_json_wraps_the_user_and_the_password() {
    let (dir, running) = admin_server().await;
    let args = [
        "user",
        "create",
        "carol",
        "--display-name",
        "Carol C",
        "--email",
        "c@example.invalid",
        "--json",
    ];
    let (result, out) = admin(dir.path(), &args, "").await;
    result.unwrap();
    let v: Value = serde_json::from_str(&out).expect("json stdout");
    assert_eq!(v["user"]["username"], "carol");
    assert_eq!(v["user"]["display_name"], "Carol C");
    assert_eq!(v["user"]["email"], "c@example.invalid");
    assert_eq!(v["user"]["server_admin"], false);
    assert!(v["password"].as_str().is_some_and(|p| p.len() == 32), "{v}");
    let (_, out) = admin(
        dir.path(),
        &["--json", "user", "create", "dave", "--password-stdin"],
        "pw-123456\n",
    )
    .await;
    let v: Value = serde_json::from_str(&out).unwrap();
    assert!(
        v["password"].is_null(),
        "a given password is not echoed: {v}"
    );
    assert_eq!(
        v["user"]["display_name"], "dave",
        "display name defaults to the username"
    );
    running.shutdown().await;
}

#[tokio::test]
async fn the_api_error_body_becomes_the_error_and_stdout_stays_empty() {
    let (dir, running) = admin_server().await;
    admin(
        dir.path(),
        &["user", "create", "ann", "--password-stdin"],
        "first-pass\n",
    )
    .await
    .0
    .unwrap();
    let (result, out) = admin(
        dir.path(),
        &["user", "create", "ann", "--password-stdin"],
        "second-pass\n",
    )
    .await;
    let err = result.unwrap_err();
    assert!(
        matches!(&err, AdminError::Api { status: 409, code, .. } if code == "conflict"),
        "got {err:?}"
    );
    let text = err.to_string();
    assert!(text.starts_with("conflict: "), "{text}");
    assert!(
        !text.contains("second-pass") && !format!("{err:?}").contains("second-pass"),
        "{text}"
    );
    assert_eq!(out, "", "nothing on stdout");
    let (result, _) = admin(dir.path(), &["user", "create", "Bad_Name"], "").await;
    let err = result.unwrap_err();
    assert!(
        matches!(&err, AdminError::Api { status: 422, field: Some(f), .. } if f == "username"),
        "got {err:?}"
    );
    assert!(err.to_string().ends_with("(field username)"), "{err}");
    running.shutdown().await;
}
