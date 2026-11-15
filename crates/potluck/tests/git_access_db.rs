//! The real resolver: tokens to roles over the database, and the hook's environment.

use std::path::PathBuf;

use potluck::db::accounts::UserPatch;
use reqwest::Method;
use serde_json::json;

use crate::common::{
    FullServer, SEED_COMMIT, api_admin, api_team, api_user, api_workspace, call, failed,
    full_server, git, ok, work_tree,
};

struct Arena {
    server: FullServer,
    home: PathBuf,
    url: String,
    root: String,
    bob: String,
    ann: String,
}

/// A server with team `core` and workspace `core/api` (apinata) made through the API as
/// `root`, plus `bob` and `ann` with tokens and no grants.
async fn arena(seed: bool) -> Arena {
    let server = full_server().await;
    let root = api_admin(server.db()).await;
    let (_, bob) = api_user(server.db(), "bob").await;
    let (_, ann) = api_user(server.db(), "ann").await;
    api_team(&server.api(""), &root, "core").await;
    api_workspace(&server.api(""), &root, "core", "api", seed).await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.git_url("core", "api");
    Arena {
        server,
        home,
        url,
        root,
        bob,
        ann,
    }
}

async fn grant(a: &Arena, ws: &str, kind: &str, name: &str, role: &str) {
    let (status, body) = call(
        &a.server
            .api(&format!("/workspaces/core/{ws}/grants/{kind}/{name}")),
        Method::PUT,
        Some(&a.root),
        Some(json!({"role": role})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clone_of_a_fresh_apinata_workspace_shows_the_seed() {
    let a = arena(true).await;
    grant(&a, "api", "user", "bob", "read").await;
    ok(git(
        &a.home,
        Some(&a.home),
        Some(&a.bob),
        &["clone", "-q", &a.url, "clone"],
    ));
    let clone = a.home.join("clone");
    assert_eq!(
        std::fs::read_to_string(clone.join(".gitignore")).unwrap(),
        ".env\n"
    );
    assert_eq!(
        std::fs::read(clone.join("opencollection.yml"))
            .unwrap()
            .len(),
        129
    );
    assert_eq!(
        std::fs::read_dir(&clone).unwrap().count(),
        3,
        ".git and the two seed files"
    );
    assert_eq!(
        ok(git(&a.home, Some(&clone), None, &["rev-parse", "HEAD"])).trim(),
        SEED_COMMIT
    );
    assert_eq!(
        ok(git(
            &a.home,
            Some(&clone),
            None,
            &["log", "--format=%an <%ae>"]
        ))
        .trim(),
        "Potluck <potluck@users.noreply.127.0.0.1>",
        "one commit, the Potluck identity"
    );
    assert_eq!(
        ok(git(
            &a.home,
            Some(&clone),
            None,
            &["branch", "--show-current"]
        ))
        .trim(),
        "main"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn roles_decide_who_fetches_and_pushes() {
    let a = arena(true).await;
    let out = failed(git(
        &a.home,
        Some(&a.home),
        Some(&a.bob),
        &["clone", "-q", &a.url, "c1"],
    ));
    assert!(out.contains("not found"), "no role is not found: {out}");
    let out = failed(git(
        &a.home,
        Some(&a.home),
        Some("plk_unknown"),
        &["clone", "-q", &a.url, "c2"],
    ));
    assert!(
        out.contains("terminal prompts disabled"),
        "an unknown token is a 401 challenge: {out}"
    );
    grant(&a, "api", "user", "bob", "read").await;
    ok(git(
        &a.home,
        Some(&a.home),
        Some(&a.bob),
        &["clone", "-q", &a.url, "clone"],
    ));
    let clone = a.home.join("clone");
    std::fs::write(clone.join("notes.md"), "hello\n").unwrap();
    ok(git(&a.home, Some(&clone), None, &["add", "notes.md"]));
    ok(git(
        &a.home,
        Some(&clone),
        None,
        &["commit", "-q", "-m", "notes"],
    ));
    let out = failed(git(
        &a.home,
        Some(&clone),
        Some(&a.bob),
        &["push", "-q", "origin", "main"],
    ));
    assert!(out.contains("403"), "read cannot push: {out}");
    let (status, _) = call(
        &a.server.api("/teams/core/members/bob"),
        Method::PUT,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(status, 200);
    grant(&a, "api", "team", "core", "write").await;
    ok(git(
        &a.home,
        Some(&clone),
        Some(&a.bob),
        &["push", "-q", "origin", "main"],
    ));
    let tip = ok(git(&a.home, Some(&clone), None, &["rev-parse", "HEAD"]));
    let refs = ok(git(
        &a.home,
        Some(&a.home),
        Some(&a.root),
        &["ls-remote", &a.url, "refs/heads/main"],
    ));
    assert!(
        refs.starts_with(tip.trim()),
        "the team grant let the push land: {refs}"
    );
    let out = failed(git(
        &a.home,
        Some(&a.home),
        Some(&a.ann),
        &["ls-remote", &a.url],
    ));
    assert!(out.contains("not found"), "ann holds nothing: {out}");
    grant(&a, "api", "user", "ann", "read").await;
    ok(git(
        &a.home,
        Some(&a.home),
        Some(&a.ann),
        &["ls-remote", &a.url],
    ));
    a.server
        .db()
        .update_user("ann", UserPatch::default().disabled(true))
        .await
        .unwrap()
        .unwrap();
    let out = failed(git(
        &a.home,
        Some(&a.home),
        Some(&a.ann),
        &["ls-remote", &a.url],
    ));
    assert!(
        out.contains("terminal prompts disabled"),
        "a disabled user's token is 401, grant or not: {out}"
    );
    let work = work_tree(&a.home, "admin-work");
    ok(git(
        &a.home,
        Some(&work),
        Some(&a.root),
        &["push", "-q", &a.url, "main:refs/heads/from-admin"],
    ));
    let (status, _) = call(
        &a.server.api("/workspaces/core/api/grants/team/core"),
        Method::DELETE,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(status, 204);
    ok(git(
        &a.home,
        Some(&clone),
        Some(&a.bob),
        &["fetch", "-q", "origin"],
    ));
    let (status, _) = call(
        &a.server.api("/workspaces/core/api/grants/user/bob"),
        Method::DELETE,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(status, 204);
    let out = failed(git(
        &a.home,
        Some(&clone),
        Some(&a.bob),
        &["fetch", "-q", "origin"],
    ));
    assert!(
        out.contains("not found"),
        "every grant gone: the clone cannot even fetch: {out}"
    );
}
