//! `potluck admin grant ...` and `override ...` on one workspace, as the system actor.

use clap::Parser;
use potluck::admin::AdminError;
use potluck::cli::Cli;
use potluck::server::Running;
use serde_json::{Value, json};

use crate::common::{admin, admin_server, columns};

fn json(out: &str) -> Value {
    serde_json::from_str(out).unwrap_or_else(|e| panic!("{e}: {out:?}"))
}

/// A server with users `bob` and `ann`, team `core` and the seeded apinata workspace `core/api`.
async fn arena() -> (tempfile::TempDir, Running) {
    let (dir, running) = admin_server().await;
    let steps: [&[&str]; 4] = [
        &["user", "create", "bob"],
        &["user", "create", "ann"],
        &["team", "create", "core"],
        &["workspace", "create", "core", "api", "--kind", "apinata"],
    ];
    for args in steps {
        admin(dir.path(), args, "").await.0.unwrap();
    }
    (dir, running)
}

#[tokio::test]
async fn grant_set_list_and_remove() {
    let (dir, running) = arena().await;
    let (result, out) = admin(
        dir.path(),
        &["--json", "grant", "set", "core/api", "user", "bob", "write"],
        "",
    )
    .await;
    result.unwrap();
    assert_eq!(
        json(&out),
        json!({"subject": {"kind": "user", "name": "bob"}, "role": "write"})
    );
    admin(
        dir.path(),
        &["grant", "set", "core/api", "team", "core", "read"],
        "",
    )
    .await
    .0
    .unwrap();
    let (result, out) = admin(dir.path(), &["grant", "list", "core/api"], "").await;
    result.unwrap();
    assert_eq!(
        columns(out.lines().next().unwrap()),
        ["SUBJECT", "ROLE"],
        "{out}"
    );
    assert!(
        out.contains("team:core") && out.contains("user:bob"),
        "{out}"
    );
    let (result, out) = admin(
        dir.path(),
        &["grant", "remove", "core/api", "user", "bob"],
        "",
    )
    .await;
    result.unwrap();
    assert_eq!(out, "removed grant user:bob from core/api\n");
    let (result, out) = admin(
        dir.path(),
        &["--json", "grant", "remove", "core/api", "team", "core"],
        "",
    )
    .await;
    result.unwrap();
    assert_eq!(out, "", "a 204 prints nothing in JSON mode");
    let err = admin(
        dir.path(),
        &["grant", "remove", "core/api", "user", "bob"],
        "",
    )
    .await
    .0
    .unwrap_err();
    assert!(
        matches!(err, AdminError::Api { status: 404, .. }),
        "{err:?}"
    );
    let err = admin(
        dir.path(),
        &["grant", "set", "core/api", "user", "nobody", "read"],
        "",
    )
    .await
    .0
    .unwrap_err();
    assert!(
        matches!(err, AdminError::Api { status: 404, .. }),
        "unknown user: {err:?}"
    );
    // The role and the subject kind are checked by clap before any request.
    let err = Cli::try_parse_from([
        "potluck", "admin", "grant", "set", "core/api", "user", "bob", "owner",
    ])
    .unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    let err = Cli::try_parse_from([
        "potluck", "admin", "grant", "set", "core/api", "group", "bob", "read",
    ])
    .unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    running.shutdown().await;
}

#[tokio::test]
async fn override_arm_list_and_disarm_record_the_system_actor() {
    let (dir, running) = arena().await;
    let args = [
        "--json",
        "override",
        "arm",
        "core/api",
        "--for-user",
        "bob",
        "--reason",
        "importing the legacy env",
    ];
    let (result, out) = admin(dir.path(), &args, "").await;
    result.unwrap();
    let v = json(&out);
    assert_eq!(v["armed_by"], "system");
    assert_eq!(v["for_user"], "bob");
    assert_eq!(v["workspace"], "core/api");
    assert!(
        v["consumed_at"].is_null() && v["consumed_refs"].is_null(),
        "{v}"
    );
    let id = v["id"].as_i64().unwrap();
    let (result, out) = admin(dir.path(), &["override", "list", "core/api"], "").await;
    result.unwrap();
    assert_eq!(
        columns(out.lines().next().unwrap()),
        [
            "ID",
            "FOR USER",
            "REASON",
            "ARMED BY",
            "ARMED AT",
            "CONSUMED AT",
            "REFS"
        ],
        "{out}"
    );
    assert!(
        out.contains("system") && out.contains("importing the legacy env"),
        "{out}"
    );
    let err = admin(
        dir.path(),
        &[
            "override",
            "arm",
            "core/api",
            "--for-user",
            "bob",
            "--reason",
            "again",
        ],
        "",
    )
    .await
    .0
    .unwrap_err();
    assert!(
        matches!(err, AdminError::Api { status: 409, .. }),
        "one armed per user: {err:?}"
    );
    let (result, out) = admin(
        dir.path(),
        &["override", "disarm", "core/api", &id.to_string()],
        "",
    )
    .await;
    result.unwrap();
    assert_eq!(out, format!("disarmed override {id}\n"));
    let err = admin(
        dir.path(),
        &["override", "disarm", "core/api", &id.to_string()],
        "",
    )
    .await
    .0
    .unwrap_err();
    assert!(
        matches!(err, AdminError::Api { status: 404, .. }),
        "{err:?}"
    );
    let (_, out) = admin(
        dir.path(),
        &[
            "--json",
            "override",
            "arm",
            "core/api",
            "--for-user",
            "bob",
            "--reason",
            "once more",
        ],
        "",
    )
    .await;
    let again = json(&out)["id"].as_i64().unwrap();
    let (result, out) = admin(
        dir.path(),
        &[
            "--json",
            "override",
            "disarm",
            "core/api",
            &again.to_string(),
        ],
        "",
    )
    .await;
    result.unwrap();
    assert_eq!(out, "", "a 204 prints nothing in JSON mode");
    let err = admin(
        dir.path(),
        &[
            "override",
            "arm",
            "core/api",
            "--for-user",
            "nobody",
            "--reason",
            "x",
        ],
        "",
    )
    .await
    .0
    .unwrap_err();
    assert!(
        matches!(&err, AdminError::Api { status: 422, field: Some(f), .. } if f == "for_user"),
        "{err:?}"
    );
    // The CLI passes the reason as given; the server refuses one that is blank once trimmed.
    let err = admin(
        dir.path(),
        &[
            "override",
            "arm",
            "core/api",
            "--for-user",
            "ann",
            "--reason",
            "   ",
        ],
        "",
    )
    .await
    .0
    .unwrap_err();
    assert!(
        matches!(&err, AdminError::Api { status: 422, field: Some(f), .. } if f == "reason"),
        "{err:?}"
    );
    running.shutdown().await;
}

#[tokio::test]
async fn a_grant_subject_that_is_not_one_path_segment_is_refused_before_any_request() {
    // No server here: a request that slipped through would be `Unreachable`, not `BadName`.
    let dir = tempfile::tempdir().unwrap();
    for (args, value) in [
        (
            &[
                "grant",
                "remove",
                "core/api",
                "user",
                "../../../../users/bob",
            ][..],
            "../../../../users/bob",
        ),
        (
            &["grant", "set", "core/api", "team", "..%2e", "read"],
            "..%2e",
        ),
        (&["grant", "remove", "core/api", "user", "a\\b"], "a\\b"),
        (
            &["grant", "set", "core/api", "user", "bob?x=1", "admin"],
            "bob?x=1",
        ),
        (&["grant", "remove", "core/api", "team", "core#x"], "core#x"),
    ] {
        let err = admin(dir.path(), args, "").await.0.unwrap_err();
        assert!(
            matches!(&err, AdminError::BadName { arg: "name", value: v, .. } if v == value),
            "{args:?}: got {err:?}"
        );
    }
}
