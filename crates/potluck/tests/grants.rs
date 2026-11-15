//! Workspace deletion and grants, both for the workspace's admins.

use reqwest::Method;
use serde_json::{Value, json};

use crate::common::{
    SEED_COMMIT, TestApp, api_admin, api_team, api_user, api_workspace, call, git, ok, repo_path,
    test_app,
};

async fn grant(app: &TestApp, token: &str, kind: &str, name: &str, role: &str) -> (u16, Value) {
    call(
        &app.url(&format!("/workspaces/core/api/grants/{kind}/{name}")),
        Method::PUT,
        Some(token),
        Some(json!({"role": role})),
    )
    .await
}

async fn ungrant(app: &TestApp, token: &str, kind: &str, name: &str) -> u16 {
    call(
        &app.url(&format!("/workspaces/core/api/grants/{kind}/{name}")),
        Method::DELETE,
        Some(token),
        None,
    )
    .await
    .0
}

async fn grants(app: &TestApp, token: &str) -> (u16, Value) {
    call(
        &app.url("/workspaces/core/api/grants"),
        Method::GET,
        Some(token),
        None,
    )
    .await
}

fn subject(kind: &str, name: &str, role: &str) -> Value {
    json!({"subject": {"kind": kind, "name": name}, "role": role})
}

#[tokio::test]
async fn admins_manage_grants_and_the_role_follows() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let api = app.url("");
    api_team(&api, &root, "core").await;
    api_team(&api, &root, "qa").await;
    api_workspace(&api, &root, "core", "api", true).await;
    let (_, bob) = api_user(&app.db, "bob").await;
    let (_, ann) = api_user(&app.db, "ann").await;
    let (_, zed) = api_user(&app.db, "zed").await;
    let (status, g) = grant(&app, &root, "user", "bob", "admin").await;
    assert_eq!((status, g), (200, subject("user", "bob", "admin")));
    assert_eq!(
        grant(&app, &bob, "team", "qa", "read").await.0,
        200,
        "bob, now admin, grants"
    );
    assert_eq!(grant(&app, &bob, "user", "ann", "write").await.0, 200);
    let (_, g) = grant(&app, &bob, "user", "ann", "read").await;
    assert_eq!(g["role"], "read", "a second put replaces the role");
    let (status, listed) = grants(&app, &bob).await;
    assert_eq!(status, 200, "{listed}");
    assert_eq!(
        listed,
        json!([
            subject("team", "qa", "read"),
            subject("user", "ann", "read"),
            subject("user", "bob", "admin")
        ])
    );
    assert_eq!(grant(&app, &bob, "user", "nobody", "read").await.0, 404);
    assert_eq!(grant(&app, &bob, "team", "nope", "read").await.0, 404);
    assert_eq!(grant(&app, &bob, "group", "bob", "read").await.0, 404);
    let (status, err) = grant(&app, &bob, "user", "ann", "owner").await;
    assert_eq!(
        (status, &err["error"]["field"]),
        (422, &json!("role")),
        "{err}"
    );
    let put_ann = app.url("/workspaces/core/api/grants/user/ann");
    assert_eq!(
        call(&put_ann, Method::PUT, Some(&bob), Some(json!({})))
            .await
            .0,
        400
    );
    assert_eq!(grants(&app, &ann).await.0, 403, "read may not see grants");
    assert_eq!(
        grant(&app, &ann, "user", "ann", "admin").await.0,
        403,
        "nor raise itself"
    );
    assert_eq!(
        call(&put_ann, Method::PUT, Some(&ann), Some(json!({})))
            .await
            .0,
        403,
        "the role is checked before the body"
    );
    assert_eq!(ungrant(&app, &ann, "user", "bob").await, 403);
    assert_eq!(grants(&app, &zed).await.0, 404, "no role: not found");
    assert_eq!(grants(&app, &zed).await.1["error"]["code"], "not_found");
    assert_eq!(
        call(
            &app.url("/workspaces/core/api/grants"),
            Method::GET,
            None,
            None
        )
        .await
        .0,
        401
    );
    assert_eq!(ungrant(&app, &bob, "user", "ann").await, 204);
    assert_eq!(ungrant(&app, &bob, "user", "ann").await, 404, "gone");
    assert_eq!(
        call(
            &app.url("/workspaces/core/api"),
            Method::GET,
            Some(&ann),
            None
        )
        .await
        .0,
        404,
        "ann lost her only grant"
    );
    assert_eq!(
        ungrant(&app, &bob, "user", "bob").await,
        204,
        "an admin may drop their own grant"
    );
    assert_eq!(grants(&app, &bob).await.0, 404);
    assert_eq!(
        grants(&app, &root).await.1,
        json!([subject("team", "qa", "read")]),
        "a server admin always sees them"
    );
    assert_eq!(
        call(
            &app.url("/workspaces/core/nope/grants"),
            Method::GET,
            Some(&root),
            None
        )
        .await
        .0,
        404
    );
}

#[tokio::test]
async fn deleting_a_workspace_removes_rows_then_the_repository() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let api = app.url("");
    api_team(&api, &root, "core").await;
    api_workspace(&api, &root, "core", "api", true).await;
    let data = app._dir.path();
    let repo = repo_path(data, "core", "api");
    assert!(repo.join("HEAD").is_file());
    let (_, bob) = api_user(&app.db, "bob").await;
    let url = app.url("/workspaces/core/api");
    grant(&app, &root, "user", "bob", "write").await;
    assert_eq!(
        call(&url, Method::DELETE, Some(&bob), None).await.0,
        403,
        "write may not delete"
    );
    grant(&app, &root, "user", "bob", "admin").await;
    let (status, body) = call(&url, Method::DELETE, Some(&bob), None).await;
    assert_eq!((status, body), (204, Value::Null));
    assert!(!repo.exists(), "the directory went with the rows");
    assert_eq!(
        app.db.workspace_by_slugs("core", "api").await.unwrap(),
        None
    );
    assert_eq!(call(&url, Method::GET, Some(&root), None).await.0, 404);
    assert_eq!(
        call(&url, Method::DELETE, Some(&root), None).await.0,
        404,
        "deleting twice"
    );
    api_workspace(&api, &root, "core", "api", true).await;
    let shown = ok(git(
        data,
        None,
        None,
        &[
            "--git-dir",
            repo.to_str().unwrap(),
            "rev-parse",
            "--verify",
            "refs/heads/main",
        ],
    ));
    assert_eq!(shown.trim(), SEED_COMMIT, "re-created afresh");
    assert_eq!(
        grants(&app, &root).await.1,
        json!([]),
        "no grant survived the deletion"
    );
    assert_eq!(
        call(&url, Method::GET, Some(&bob), None).await.0,
        404,
        "bob's old admin grant is gone"
    );
    std::fs::remove_dir_all(&repo).unwrap();
    assert_eq!(
        call(&url, Method::DELETE, Some(&root), None).await.0,
        204,
        "a directory already gone does not block deletion"
    );
    assert_eq!(
        app.db.workspace_by_slugs("core", "api").await.unwrap(),
        None
    );
    assert_eq!(call(&url, Method::DELETE, None, None).await.0, 401);
    assert_eq!(
        call(
            &app.url("/workspaces/core/nope"),
            Method::DELETE,
            Some(&root),
            None
        )
        .await
        .0,
        404
    );
    assert!(repo.parent().unwrap().is_dir(), "the team directory stays");
}

#[tokio::test]
async fn a_failed_removal_is_500_with_the_workspace_already_gone() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let api = app.url("");
    api_team(&api, &root, "core").await;
    api_workspace(&api, &root, "core", "api", true).await;
    // A regular file where the repository was makes the directory removal fail.
    let repo = repo_path(app._dir.path(), "core", "api");
    std::fs::remove_dir_all(&repo).unwrap();
    std::fs::write(&repo, b"not a repository").unwrap();
    let (status, body) = call(
        &app.url("/workspaces/core/api"),
        Method::DELETE,
        Some(&root),
        None,
    )
    .await;
    assert_eq!(
        (status, &body["error"]["message"]),
        (500, &json!("repository could not be removed")),
        "{body}"
    );
    assert_eq!(
        app.db.workspace_by_slugs("core", "api").await.unwrap(),
        None,
        "the row went first"
    );
    assert!(repo.is_file(), "the leftover is the operator's");
}
