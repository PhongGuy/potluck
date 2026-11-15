//! `potluck admin user set|reset-password|tokens` and `token revoke`.

use potluck::admin::AdminError;
use serde_json::Value;

use crate::common::{admin, admin_server, columns, session};

fn json(out: &str) -> Value {
    serde_json::from_str(out).unwrap_or_else(|e| panic!("{e}: {out:?}"))
}

fn password_line(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("password: "))
        .map(str::to_owned)
}

#[tokio::test]
async fn user_set_changes_each_field_and_disabling_stops_sign_in() {
    let (dir, running) = admin_server().await;
    let addr = running.http_addr();
    admin(
        dir.path(),
        &[
            "user",
            "create",
            "ann",
            "--email",
            "a@example.invalid",
            "--password-stdin",
        ],
        "pw-123456\n",
    )
    .await
    .0
    .unwrap();
    let (result, out) = admin(
        dir.path(),
        &[
            "--json",
            "user",
            "set",
            "ann",
            "--display-name",
            "Ann A",
            "--server-admin",
            "true",
        ],
        "",
    )
    .await;
    result.unwrap();
    let v = json(&out);
    assert_eq!(v["display_name"], "Ann A");
    assert_eq!(v["server_admin"], true);
    assert_eq!(v["email"], "a@example.invalid", "untouched");
    let (_, out) = admin(
        dir.path(),
        &["--json", "user", "set", "ann", "--clear-email"],
        "",
    )
    .await;
    assert!(json(&out)["email"].is_null(), "{out}");
    let (result, out) = admin(
        dir.path(),
        &["--json", "user", "set", "ann", "--disabled", "true"],
        "",
    )
    .await;
    result.unwrap();
    assert_eq!(json(&out)["disabled"], true);
    assert_eq!(
        session(addr, "ann", "pw-123456").await,
        Err(401),
        "disabled"
    );
    admin(
        dir.path(),
        &["user", "set", "ann", "--disabled", "false"],
        "",
    )
    .await
    .0
    .unwrap();
    session(addr, "ann", "pw-123456")
        .await
        .expect("enabled again");
    let err = admin(
        dir.path(),
        &["user", "set", "nobody", "--disabled", "true"],
        "",
    )
    .await
    .0
    .unwrap_err();
    assert!(
        matches!(err, AdminError::Api { status: 404, .. }),
        "got {err:?}"
    );
    let (result, out) = admin(dir.path(), &["user", "set", "ann"], "").await;
    result.unwrap();
    assert!(
        out.contains("Ann A"),
        "no flags is a no-op that prints the user: {out}"
    );
    running.shutdown().await;
}

#[tokio::test]
async fn reset_password_generates_or_reads_one_and_the_old_stops_working() {
    let (dir, running) = admin_server().await;
    let addr = running.http_addr();
    admin(
        dir.path(),
        &["user", "create", "ann", "--password-stdin"],
        "old-pass-1\n",
    )
    .await
    .0
    .unwrap();
    let (result, out) = admin(dir.path(), &["user", "reset-password", "ann"], "").await;
    result.unwrap();
    let new = password_line(&out).expect("a password line");
    assert_eq!(columns(out.lines().next().unwrap())[0], "USERNAME", "{out}");
    assert_eq!(session(addr, "ann", "old-pass-1").await, Err(401));
    session(addr, "ann", &new).await.expect("the new password");
    let (result, out) = admin(
        dir.path(),
        &["user", "reset-password", "ann", "--password-stdin"],
        "newer-pass-2\n",
    )
    .await;
    result.unwrap();
    assert!(
        !out.contains("newer-pass-2") && !out.contains("password:"),
        "{out}"
    );
    session(addr, "ann", "newer-pass-2")
        .await
        .expect("the given password");
    let (_, out) = admin(dir.path(), &["--json", "user", "reset-password", "ann"], "").await;
    let v = json(&out);
    assert_eq!(v["user"]["username"], "ann");
    assert!(v["password"].as_str().is_some_and(|p| p.len() == 32), "{v}");
    running.shutdown().await;
}

#[tokio::test]
async fn user_tokens_lists_and_token_revoke_answers_quietly() {
    let (dir, running) = admin_server().await;
    let addr = running.http_addr();
    admin(
        dir.path(),
        &["user", "create", "ann", "--password-stdin"],
        "pw-123456\n",
    )
    .await
    .0
    .unwrap();
    let id = session(addr, "ann", "pw-123456").await.unwrap();
    let (result, out) = admin(dir.path(), &["user", "tokens", "ann"], "").await;
    result.unwrap();
    assert_eq!(
        columns(out.lines().next().unwrap()),
        ["ID", "NAME", "PREFIX", "CREATED", "LAST USED", "EXPIRES"],
        "{out}"
    );
    assert!(out.contains("cli-test"), "{out}");
    assert!(
        !out.split_whitespace()
            .any(|w| w.starts_with("plk_") && w.len() > 8),
        "only the 8-character prefix is shown: {out}"
    );
    let (result, out) = admin(dir.path(), &["token", "revoke", &id.to_string()], "").await;
    result.unwrap();
    assert_eq!(out, format!("revoked token {id}\n"));
    let (_, out) = admin(dir.path(), &["--json", "user", "tokens", "ann"], "").await;
    assert_eq!(json(&out), Value::Array(vec![]));
    let (result, out) = admin(
        dir.path(),
        &["--json", "token", "revoke", &id.to_string()],
        "",
    )
    .await;
    assert!(matches!(
        result.unwrap_err(),
        AdminError::Api { status: 404, .. }
    ));
    assert_eq!(out, "");
    let id = session(addr, "ann", "pw-123456").await.unwrap();
    let (result, out) = admin(
        dir.path(),
        &["--json", "token", "revoke", &id.to_string()],
        "",
    )
    .await;
    result.unwrap();
    assert_eq!(out, "", "a 204 prints nothing in JSON mode");
    let err = admin(dir.path(), &["user", "tokens", "nobody"], "")
        .await
        .0
        .unwrap_err();
    assert!(
        matches!(err, AdminError::Api { status: 404, .. }),
        "got {err:?}"
    );
    running.shutdown().await;
}
