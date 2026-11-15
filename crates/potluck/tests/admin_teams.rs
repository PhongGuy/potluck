//! `potluck admin team ...` and `workspace ...` against the team and workspace endpoints.

use clap::Parser;
use potluck::admin::AdminError;
use potluck::cli::Cli;
use serde_json::{Value, json};

use crate::common::{admin, admin_server, columns};

fn json(out: &str) -> Value {
    serde_json::from_str(out).unwrap_or_else(|e| panic!("{e}: {out:?}"))
}

#[tokio::test]
async fn team_create_list_and_members() {
    let (dir, running) = admin_server().await;
    admin(dir.path(), &["user", "create", "bob"], "")
        .await
        .0
        .unwrap();
    let (result, out) = admin(
        dir.path(),
        &["--json", "team", "create", "core", "--name", "Core team"],
        "",
    )
    .await;
    result.unwrap();
    let v = json(&out);
    assert_eq!(v["slug"], "core");
    assert_eq!(v["name"], "Core team");
    assert_eq!(v["members"], json!([]));
    let (result, out) = admin(
        dir.path(),
        &["--json", "team", "add-member", "core", "bob"],
        "",
    )
    .await;
    result.unwrap();
    assert_eq!(json(&out)["members"], json!(["bob"]));
    let (result, out) = admin(dir.path(), &["team", "list"], "").await;
    result.unwrap();
    assert_eq!(
        columns(out.lines().next().unwrap()),
        ["SLUG", "NAME", "MEMBERS"],
        "{out}"
    );
    assert!(out.contains("core") && out.contains("bob"), "{out}");
    let (result, out) = admin(dir.path(), &["team", "remove-member", "core", "bob"], "").await;
    result.unwrap();
    assert_eq!(out, "removed bob from team core\n");
    admin(dir.path(), &["team", "add-member", "core", "bob"], "")
        .await
        .0
        .unwrap();
    let (result, out) = admin(
        dir.path(),
        &["--json", "team", "remove-member", "core", "bob"],
        "",
    )
    .await;
    result.unwrap();
    assert_eq!(out, "", "a 204 prints nothing in JSON mode");
    let err = admin(dir.path(), &["team", "remove-member", "core", "bob"], "")
        .await
        .0
        .unwrap_err();
    assert!(
        matches!(err, AdminError::Api { status: 404, .. }),
        "{err:?}"
    );
    let (_, out) = admin(dir.path(), &["--json", "team", "create", "ops"], "").await;
    assert_eq!(json(&out)["name"], "ops", "the name defaults to the slug");
    running.shutdown().await;
}

#[tokio::test]
async fn workspace_create_list_show_and_delete() {
    let (dir, running) = admin_server().await;
    admin(dir.path(), &["team", "create", "core"], "")
        .await
        .0
        .unwrap();
    let (result, out) = admin(
        dir.path(),
        &[
            "--json",
            "workspace",
            "create",
            "core",
            "api",
            "--kind",
            "apinata",
        ],
        "",
    )
    .await;
    result.unwrap();
    let v = json(&out);
    assert_eq!(v["team"], "core");
    assert_eq!(v["slug"], "api");
    assert_eq!(v["name"], "api", "the name defaults to the slug");
    assert_eq!(v["kind"], "apinata");
    assert_eq!(v["default_branch"], "main");
    assert_eq!(v["role"], "admin", "system acts as admin");
    let repo = dir.path().join("repos/core/api.git");
    assert!(
        repo.join("HEAD").is_file(),
        "the repository was initialised"
    );
    let args = [
        "workspace",
        "create",
        "core",
        "legacy",
        "--kind",
        "apinata",
        "--name",
        "Legacy",
        "--default-branch",
        "trunk",
        "--import",
    ];
    admin(dir.path(), &args, "").await.0.unwrap();
    let (result, out) = admin(dir.path(), &["workspace", "list"], "").await;
    result.unwrap();
    assert_eq!(
        columns(out.lines().next().unwrap()),
        ["WORKSPACE", "NAME", "KIND", "BRANCH", "ROLE", "GIT URL"],
        "{out}"
    );
    assert!(
        out.contains("core/api") && out.contains("core/legacy") && out.contains("trunk"),
        "{out}"
    );
    let (_, out) = admin(
        dir.path(),
        &["--json", "workspace", "list", "--kind", "dbento"],
        "",
    )
    .await;
    assert_eq!(json(&out), json!([]), "an unknown kind filters to nothing");
    let (result, out) = admin(dir.path(), &["--json", "workspace", "show", "core/api"], "").await;
    result.unwrap();
    assert_eq!(
        json(&out)["git_url"],
        "http://127.0.0.1:8080/git/core/api.git"
    );
    let (result, out) = admin(dir.path(), &["workspace", "delete", "core/api"], "").await;
    result.unwrap();
    assert_eq!(out, "deleted workspace core/api\n");
    assert!(!repo.exists(), "the directory went with the row");
    let (result, out) = admin(
        dir.path(),
        &["--json", "workspace", "delete", "core/legacy"],
        "",
    )
    .await;
    result.unwrap();
    assert_eq!(out, "", "a 204 prints nothing in JSON mode");
    let err = admin(dir.path(), &["workspace", "show", "core/api"], "")
        .await
        .0
        .unwrap_err();
    assert!(
        matches!(err, AdminError::Api { status: 404, .. }),
        "{err:?}"
    );
    running.shutdown().await;
}

#[test]
fn a_workspace_reference_needs_exactly_one_slash() {
    for bad in ["core", "core/", "/api", "a/b/c"] {
        let err =
            Cli::try_parse_from(["potluck", "admin", "workspace", "show", bad]).expect_err(bad);
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ValueValidation,
            "{bad}: {err}"
        );
    }
    Cli::try_parse_from(["potluck", "admin", "workspace", "show", "core/api"])
        .expect("a good reference");
}

#[test]
fn a_workspace_reference_whose_halves_are_not_names_is_refused() {
    for bad in [
        "core/..",
        "../api",
        "core/%2e%2e",
        "co\\re/api",
        "core/api?x=1",
        "core/api#x",
    ] {
        let err =
            Cli::try_parse_from(["potluck", "admin", "workspace", "delete", bad]).expect_err(bad);
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ValueValidation,
            "{bad}: {err}"
        );
    }
}

#[tokio::test]
async fn a_team_or_member_that_is_not_one_path_segment_is_refused_before_any_request() {
    // No server here: a request that slipped through would be `Unreachable`, not `BadName`.
    let dir = tempfile::tempdir().unwrap();
    for (args, arg, value) in [
        (
            &["team", "remove-member", "t", "../../../workspaces/t/w"][..],
            "username",
            "../../../workspaces/t/w",
        ),
        (&["team", "add-member", "t", "..%2e"], "username", "..%2e"),
        (&["team", "add-member", "..", "bob"], "team", ".."),
        (&["team", "remove-member", "a\\b", "bob"], "team", "a\\b"),
        (
            &["workspace", "create", "t?x=1", "api", "--kind", "apinata"],
            "team",
            "t?x=1",
        ),
    ] {
        let err = admin(dir.path(), args, "").await.0.unwrap_err();
        assert!(
            matches!(&err, AdminError::BadName { arg: a, value: v, .. } if *a == arg && v == value),
            "{args:?}: got {err:?}"
        );
    }
}
