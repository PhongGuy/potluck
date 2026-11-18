//! Workspace creation: the seed, the import state, validation, and the orphan directory.

use std::path::Path;
use std::time::Duration;

use potluck::server::start;
use reqwest::Method;
use serde_json::{Value, json};

use crate::common::{
    SEED_COMMIT, TestApp, api_admin, api_team, api_user, call, git, hanging_git, hung_git_is_gone,
    ok, parse_config, repo_path, test_app,
};

/// `git --git-dir=<repo> <args>`; panics with stderr on failure.
fn in_repo(home: &Path, repo: &Path, args: &[&str]) -> String {
    let mut full = vec!["--git-dir", repo.to_str().unwrap()];
    full.extend_from_slice(args);
    ok(git(home, None, None, &full))
}

fn good() -> Value {
    json!({"slug": "api", "name": "API", "kind": "apinata", "default_branch": "main"})
}

async fn create(app: &TestApp, token: &str, team: &str, body: Value) -> (u16, Value) {
    call(
        &app.url(&format!("/teams/{team}/workspaces")),
        Method::POST,
        Some(token),
        Some(body),
    )
    .await
}

#[tokio::test]
async fn creating_a_workspace_seeds_the_repository_and_answers_the_object() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_team(&app.url(""), &root, "core").await;
    let (status, body) = create(&app, &root, "core", good()).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        body,
        json!({"team": "core", "slug": "api", "name": "API", "kind": "apinata", "default_branch": "main", "role": "admin",
        "git_url": "http://127.0.0.1:8080/git/core/api.git", "events_url": "http://127.0.0.1:8080/api/v1/workspaces/core/api/events"})
    );
    let data = app._dir.path();
    let repo = repo_path(data, "core", "api");
    assert_eq!(
        in_repo(data, &repo, &["rev-parse", "--verify", "refs/heads/main"]).trim(),
        SEED_COMMIT,
        "the seed commit, Potluck identity, fixed date"
    );
    assert_eq!(
        in_repo(data, &repo, &["ls-tree", "--name-only", "refs/heads/main"]),
        ".gitignore\nopencollection.yml\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("HEAD")).unwrap().trim(),
        "ref: refs/heads/main"
    );
    let ws = app
        .db
        .workspace_by_slugs("core", "api")
        .await
        .unwrap()
        .unwrap();
    assert!(!ws.import_pending());
    let (status, err) = create(&app, &root, "core", good()).await;
    assert_eq!(status, 409, "{err}");
    assert_eq!(err["error"]["code"], "conflict");
    assert_eq!(
        in_repo(data, &repo, &["rev-parse", "--verify", "refs/heads/main"]).trim(),
        SEED_COMMIT,
        "the conflict touched nothing"
    );
}

#[tokio::test]
async fn an_import_workspace_starts_empty_with_the_import_state_set() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_team(&app.url(""), &root, "core").await;
    let mut body = good();
    body["seed"] = json!(false);
    body["default_branch"] = json!("release.1");
    let (status, ws) = create(&app, &root, "core", body).await;
    assert_eq!(status, 201, "{ws}");
    assert_eq!(
        ws["default_branch"], "release.1",
        "a dot inside the name is fine"
    );
    let data = app._dir.path();
    let repo = repo_path(data, "core", "api");
    assert_eq!(
        in_repo(data, &repo, &["for-each-ref"]),
        "",
        "no refs: the import brings the history"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("HEAD")).unwrap().trim(),
        "ref: refs/heads/release.1"
    );
    assert!(
        app.db
            .workspace_by_slugs("core", "api")
            .await
            .unwrap()
            .unwrap()
            .import_pending()
    );
}

#[tokio::test]
async fn creation_is_validated_and_leaves_no_directory_behind() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_team(&app.url(""), &root, "core").await;
    for (field, value) in [
        ("slug", json!("Bad")),
        ("slug", json!("-x")),
        ("name", json!("  ")),
        ("name", json!("n".repeat(201))),
        ("kind", json!("nope")),
        ("default_branch", json!("")),
        ("default_branch", json!("-x")),
        ("default_branch", json!(".a")),
        ("default_branch", json!("a.")),
        ("default_branch", json!("a..b")),
        ("default_branch", json!("x.lock")),
        ("default_branch", json!("feature/x")),
        ("default_branch", json!("a b")),
        ("default_branch", json!("a@{b")),
        ("default_branch", json!("HEAD")),
    ] {
        let mut body = good();
        body[field] = value.clone();
        let (status, err) = create(&app, &root, "core", body).await;
        assert_eq!(status, 422, "{field}={value}: {err}");
        assert_eq!(err["error"]["field"], field, "{err}");
    }
    assert!(
        !repo_path(app._dir.path(), "core", "api").exists(),
        "no directory was claimed for a refused request"
    );
    assert!(app.db.list_workspaces().await.unwrap().is_empty());
    let (status, _) = create(&app, &root, "nope", good()).await;
    assert_eq!(status, 404, "unknown team");
    let (status, _) = create(&app, &root, "core", json!({"slug": "api"})).await;
    assert_eq!(status, 400, "missing fields");
    let (_, bob) = api_user(&app.db, "bob").await;
    let (status, _) = create(&app, &bob, "core", good()).await;
    assert_eq!(status, 403);
    let (status, _) = create(&app, &bob, "core", json!({})).await;
    assert_eq!(status, 403, "the role is checked before the body");
    let (status, _) = call(
        &app.url("/teams/core/workspaces"),
        Method::POST,
        None,
        Some(good()),
    )
    .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn an_orphan_directory_is_a_conflict() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_team(&app.url(""), &root, "core").await;
    std::fs::create_dir_all(repo_path(app._dir.path(), "core", "api")).unwrap();
    let (status, err) = create(&app, &root, "core", good()).await;
    assert_eq!(status, 409, "{err}");
    assert!(
        repo_path(app._dir.path(), "core", "api").exists(),
        "the orphan is the operator's and stays"
    );
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("directory"),
        "{err}"
    );
    assert_eq!(
        app.db.workspace_by_slugs("core", "api").await.unwrap(),
        None,
        "no row for a workspace that could not be made"
    );
}

#[tokio::test]
async fn a_failure_after_the_claim_removes_the_directory() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_team(&app.url(""), &root, "core").await;
    let (status, body) = create(&app, &root, "core", good()).await;
    assert_eq!(status, 201, "{body}");
    // With the repository gone and the row kept, the next create claims the directory and
    // then loses on the row.
    let repo = repo_path(app._dir.path(), "core", "api");
    std::fs::remove_dir_all(&repo).unwrap();
    let (status, err) = create(&app, &root, "core", good()).await;
    assert_eq!(status, 409, "{err}");
    assert_eq!(err["error"]["code"], "conflict");
    assert!(!repo.exists(), "the claimed directory was removed again");
}

#[tokio::test]
async fn a_hung_seed_git_is_killed_at_the_run_limit_and_does_not_hold_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let git = hanging_git(dir.path(), "hash-object");
    let data = dir.path().join("data");
    let config = parse_config(&[
        "--listen",
        "127.0.0.1:0",
        "--data",
        data.to_str().unwrap(),
        "--public-url",
        "http://127.0.0.1:8080",
        "--git",
        git.to_str().unwrap(),
        "--git-max-runtime-secs",
        "1",
    ])
    .unwrap();
    let running = start(&config).await.unwrap();
    let api = format!("http://{}/api/v1", running.http_addr());
    let token = api_admin(running.db()).await;
    api_team(&api, &token, "acme").await;
    let body = json!({"slug": "ws", "name": "ws", "kind": "apinata", "default_branch": "main", "seed": true});
    let url = format!("{api}/teams/acme/workspaces");
    let create = call(&url, Method::POST, Some(&token), Some(body));
    // Both waits are capped well past the 1 s limit, so a regression fails instead of hanging.
    let created = tokio::time::timeout(Duration::from_secs(15), create).await;
    let stopped = tokio::time::timeout(Duration::from_secs(15), running.shutdown()).await;
    let (status, body) = created.expect("the workspace create outlived the git run limit");
    assert_eq!(status, 500, "{body}");
    assert!(stopped.is_ok(), "shutdown waited on the hung seed git");
    assert!(
        !repo_path(&data, "acme", "ws").exists(),
        "the half-made repository is removed"
    );
    assert!(
        hung_git_is_gone(dir.path()),
        "the hung git and its group child are killed and reaped"
    );
}
