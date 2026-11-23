//! The real resolver: tokens to roles over the database, and the hook's environment.

use std::path::PathBuf;

use potluck::db::accounts::UserPatch;
use potluck::db::audit::AuditCtx;
use reqwest::Method;
use serde_json::json;

use crate::common::{
    FullServer, SEED_COMMIT, api_admin, api_team, api_user, api_workspace, call, client, failed,
    full_server, git, ok, work_tree, write_hook,
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
        .update_user(
            "ann",
            UserPatch::default().disabled(true),
            &AuditCtx::system(),
        )
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

/// The test hook's one `remote: kind=` line, without the padding git appends to remote lines,
/// so a value can be compared to its end and a stray `override=999` cannot pass.
fn hook_line(out: &str) -> &str {
    out.lines()
        .find(|line| line.starts_with("remote: kind="))
        .map_or("", str::trim_end)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hook_sees_kind_pusher_token_and_flags() {
    let a = arena(false).await;
    grant(&a, "api", "user", "bob", "write").await;
    write_hook(
        &a.server.hooks_dir(),
        "echo \"kind=$POTLUCK_KIND role=$POTLUCK_ROLE pusher=$POTLUCK_PUSHER token=$POTLUCK_PUSHER_TOKEN import=$POTLUCK_IMPORT override=$POTLUCK_OVERRIDE\" >&2\nexit 1",
    );
    let work = work_tree(&a.home, "work");
    let bob = a
        .server
        .db()
        .user_by_username("bob")
        .await
        .unwrap()
        .unwrap();
    let token_id = a.server.db().tokens_for_user(bob.id()).await.unwrap()[0].id();
    let out = failed(git(
        &a.home,
        Some(&work),
        Some(&a.bob),
        &["push", "--porcelain", &a.url, "main"],
    ));
    assert_eq!(
        hook_line(&out),
        format!("remote: kind=apinata role=write pusher=bob token={token_id} import=1 override="),
        "{out}"
    );
    let ws = a
        .server
        .db()
        .workspace_by_slugs("core", "api")
        .await
        .unwrap()
        .unwrap();
    let id = a
        .server
        .db()
        .arm_override(ws.id(), bob.id(), "import", "system")
        .await
        .unwrap();
    let out = failed(git(
        &a.home,
        Some(&work),
        Some(&a.bob),
        &["push", "--porcelain", &a.url, "main"],
    ));
    assert_eq!(
        hook_line(&out),
        format!(
            "remote: kind=apinata role=write pusher=bob token={token_id} import=1 override={id}"
        ),
        "an armed override reaches the hook: {out}"
    );
    api_workspace(&a.server.api(""), &a.root, "core", "docs", true).await;
    grant(&a, "docs", "user", "bob", "write").await;
    let docs = a.server.git_url("core", "docs");
    let out = failed(git(
        &a.home,
        Some(&work),
        Some(&a.bob),
        &["push", "--porcelain", &docs, "main:refs/heads/extra"],
    ));
    assert_eq!(
        hook_line(&out),
        format!("remote: kind=apinata role=write pusher=bob token={token_id} import= override="),
        "a seeded workspace has no import or override flag: {out}"
    );
    assert_eq!(
        ok(git(
            &a.home,
            Some(&a.home),
            Some(&a.root),
            &["ls-remote", &a.url]
        )),
        "",
        "the declining hook let nothing land"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hook_sees_role_admin_for_a_server_admin_and_an_admin_grant() {
    let a = arena(false).await;
    grant(&a, "api", "user", "ann", "admin").await;
    write_hook(
        &a.server.hooks_dir(),
        "echo \"kind=$POTLUCK_KIND role=$POTLUCK_ROLE pusher=$POTLUCK_PUSHER\" >&2\nexit 1",
    );
    let work = work_tree(&a.home, "work");
    for (who, token) in [("root", &a.root), ("ann", &a.ann)] {
        let out = failed(git(
            &a.home,
            Some(&work),
            Some(token),
            &["push", "--porcelain", &a.url, "main"],
        ));
        assert_eq!(
            hook_line(&out),
            format!("remote: kind=apinata role=admin pusher={who}"),
            "{out}"
        );
    }
}

/// Over git too, a workspace the caller holds no role on answers byte for byte like one that
/// does not exist.
#[tokio::test]
async fn info_refs_answers_no_role_exactly_like_no_workspace() {
    let a = arena(false).await;
    let mut answers = Vec::new();
    for (team, ws) in [("core", "api"), ("core", "nope"), ("other", "api")] {
        let res = client()
            .get(format!(
                "{}/info/refs?service=git-upload-pack",
                a.server.git_url(team, ws)
            ))
            .bearer_auth(&a.bob)
            .send()
            .await
            .unwrap();
        let status = res.status().as_u16();
        let content_type = res.headers().get("content-type").cloned();
        answers.push((status, content_type, res.bytes().await.unwrap()));
    }
    assert_eq!(answers[0].0, 404, "{:?}", answers[0]);
    assert_eq!(answers[0], answers[1], "no role and an unknown workspace");
    assert_eq!(answers[0], answers[2], "no role and an unknown team");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hook_sees_the_workspace_default_branch() {
    let a = arena(false).await;
    let (status, body) = call(
        &a.server.api("/teams/core/workspaces"),
        Method::POST,
        Some(&a.root),
        Some(json!({"slug": "trunked", "name": "trunked", "kind": "apinata", "default_branch": "trunk", "seed": true})),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    grant(&a, "trunked", "user", "bob", "write").await;
    write_hook(
        &a.server.hooks_dir(),
        "echo \"branch=$POTLUCK_DEFAULT_BRANCH\" >&2\nexit 1",
    );
    let work = work_tree(&a.home, "work");
    let out = failed(git(
        &a.home,
        Some(&work),
        Some(&a.bob),
        &[
            "push",
            "--porcelain",
            &a.server.git_url("core", "trunked"),
            "main:refs/heads/extra",
        ],
    ));
    assert!(
        out.lines().any(|l| l.trim_end() == "remote: branch=trunk"),
        "{out}"
    );
}
