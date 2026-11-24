//! Ref namespaces: a non-admin may write only branches and tags.

use std::path::PathBuf;
use std::time::Duration;

use potluck::role::Role;
use potluck::secret_check::git::{Git, RefUpdate};
use potluck::secret_check::scan::{HookEnv, Rejection, foreign_refs, scan};
use tokio::time::timeout;

use crate::common::scratch::Scratch;
use crate::common::{
    APINATA_FORMAT, FullServer, api_admin, api_team, api_user, api_workspace, call, failed,
    full_server, git, ok, work_tree, write_script,
};

const ZERO: &str = "0000000000000000000000000000000000000000";

fn git_for(s: &Scratch) -> Git {
    Git::new(s.isolated_git(), Some(s.bare.clone()))
}

fn moved(old: &str, new: &str, name: &str) -> RefUpdate {
    RefUpdate::parse_line(format!("{old} {new} {name}").as_bytes()).unwrap()
}

/// A write-role pusher on a workspace whose default branch is `main`.
fn writer() -> HookEnv {
    HookEnv::new(None, false, None).with_default_branch("main")
}

/// `"<ref> <path> <code> <rule>"` per rejection, in order.
fn rows(r: &[Rejection]) -> Vec<String> {
    r.iter()
        .map(|x| {
            format!(
                "{} {} {} {}",
                String::from_utf8_lossy(x.ref_name()),
                String::from_utf8_lossy(x.path()),
                x.finding().code().as_str(),
                x.finding().rule()
            )
        })
        .collect()
}

fn line(name: &str) -> String {
    format!("{name} / safety_relaxation ref_namespace")
}

/// A commit on the server's `main`, so the default branch exists, ready to be pushed anywhere.
fn staged(s: &Scratch) -> String {
    s.write("a.txt", b"one\n");
    let tip = s.commit("one");
    s.publish("main");
    tip
}

#[test]
fn a_writer_creating_a_notes_ref_is_refused_with_one_ref_namespace_line() {
    let s = Scratch::new();
    let tip = staged(&s);
    let found = scan(
        &writer(),
        &[moved(ZERO, &tip, "refs/notes/x")],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(rows(&found), [line("refs/notes/x")]);
}

#[test]
fn a_writer_delete_outside_heads_and_tags_is_refused_without_any_git_call() {
    let s = Scratch::new();
    let tip = staged(&s);
    let broken = Git::new(PathBuf::from("/nonexistent/git"), Some(s.bare.clone()));
    let delete = || moved(&tip, ZERO, "refs/foo/x");
    let found = scan(&writer(), &[delete()], &broken).unwrap();
    assert_eq!(rows(&found), [line("refs/foo/x")]);
    // An unknown kind skips the kind lookup for an all-delete push, but not this rule.
    let unknown = HookEnv::new(Some("no-such-kind"), false, None).with_default_branch("main");
    let found = scan(&unknown, &[delete()], &broken).unwrap();
    assert_eq!(rows(&found), [line("refs/foo/x")]);
}

#[test]
fn a_prefix_without_its_trailing_slash_is_refused() {
    let s = Scratch::new();
    let tip = staged(&s);
    let git = git_for(&s);
    for name in ["refs/headsx/y", "refs/tagsx/y"] {
        let found = scan(&writer(), &[moved(ZERO, &tip, name)], &git).unwrap();
        assert_eq!(rows(&found), [line(name)], "{name}");
    }
}

#[test]
fn a_writer_may_create_a_branch_and_a_tag() {
    let s = Scratch::new();
    let tip = staged(&s);
    let git = git_for(&s);
    for name in ["refs/heads/t", "refs/tags/v1"] {
        let found = scan(&writer(), &[moved(ZERO, &tip, name)], &git).unwrap();
        assert!(found.is_empty(), "{name}: {:?}", rows(&found));
    }
}

#[test]
fn an_admin_or_an_armed_override_may_write_any_ref() {
    let s = Scratch::new();
    let tip = staged(&s);
    let git = git_for(&s);
    let admin = writer().with_role(Role::Admin);
    let armed = HookEnv::new(None, false, Some("9")).with_default_branch("main");
    for env in [admin, armed] {
        let found = scan(&env, &[moved(ZERO, &tip, "refs/notes/x")], &git).unwrap();
        assert!(found.is_empty(), "{env:?}: {:?}", rows(&found));
        let found = scan(&env, &[moved(&tip, ZERO, "refs/foo/x")], &git).unwrap();
        assert!(found.is_empty(), "{env:?}: {:?}", rows(&found));
    }
}

#[test]
fn only_the_foreign_ref_of_a_mixed_push_is_refused() {
    let s = Scratch::new();
    let tip = staged(&s);
    let found = scan(
        &writer(),
        &[
            moved(ZERO, &tip, "refs/heads/t"),
            moved(ZERO, &tip, "refs/remotes/origin/t"),
        ],
        &git_for(&s),
    )
    .unwrap();
    assert_eq!(rows(&found), [line("refs/remotes/origin/t")]);
}

#[test]
fn foreign_refs_lists_each_foreign_update_in_order() {
    let sha = "1111111111111111111111111111111111111111";
    let updates = [
        moved(ZERO, sha, "refs/remotes/origin/main"),
        moved(ZERO, sha, "refs/heads/main"),
        moved(sha, ZERO, "refs/notes/commits"),
        moved(ZERO, sha, "refs/tags/v1"),
        moved(sha, sha, "refs/pull/1/head"),
    ];
    assert_eq!(
        rows(&foreign_refs(&updates)),
        [
            line("refs/remotes/origin/main"),
            line("refs/notes/commits"),
            line("refs/pull/1/head"),
        ]
    );
}

#[test]
fn an_accepted_ref_outside_heads_and_tags_is_logged_with_workspace_pusher_and_override() {
    let sha = "1111111111111111111111111111111111111111";
    let log = |name: &str| {
        crate::logs::captured_logs(|| {
            potluck::secret_check::log_foreign_refs(7, "alice", Some(3), &[moved(ZERO, sha, name)]);
        })
    };
    let text = log("refs/notes/x");
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("ref outside heads and tags written"))
        .collect();
    assert_eq!(lines.len(), 1, "{text}");
    for needle in [
        " INFO ",
        "workspace_id=7",
        "alice",
        "override_id=3",
        "refs/notes/x",
    ] {
        assert!(lines[0].contains(needle), "{needle} in {}", lines[0]);
    }
    for name in ["refs/heads/t", "refs/tags/v1"] {
        let text = log(name);
        assert!(
            !text.contains("ref outside heads and tags written"),
            "{name}: {text}"
        );
    }
}

/// A full server whose import workspace `core/api` runs the real secret check, and a writer's
/// token on it; returns the server, the writer's token and a scratch home.
async fn import_server() -> (FullServer, String, PathBuf) {
    let server = full_server().await;
    write_script(
        &server.hooks_dir().join("pre-receive"),
        &potluck::hooks::script(
            std::path::Path::new(env!("CARGO_BIN_EXE_potluck")),
            std::path::Path::new("git"),
        )
        .unwrap(),
    );
    let root = api_admin(server.db()).await;
    let (_, bob) = api_user(server.db(), "bob").await;
    api_team(&server.api(""), &root, "core").await;
    api_workspace(&server.api(""), &root, "core", "api", false).await;
    let (status, _) = call(
        &server.api("/workspaces/core/api/grants/user/bob"),
        reqwest::Method::PUT,
        Some(&root),
        Some(serde_json::json!({"role": "write"})),
    )
    .await;
    assert_eq!(status, 200);
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    (server, bob, home)
}

async fn import_pending(server: &FullServer) -> bool {
    server
        .db()
        .workspace_by_slugs("core", "api")
        .await
        .unwrap()
        .unwrap()
        .import_pending()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_writer_mirror_import_is_refused_and_a_heads_and_tags_import_lands() {
    let (server, bob, home) = import_server().await;
    let upstream = work_tree(&home, "upstream");
    std::fs::write(upstream.join("opencollection.yml"), APINATA_FORMAT).unwrap();
    ok(git(&home, Some(&upstream), None, &["add", "-A"]));
    ok(git(
        &home,
        Some(&upstream),
        None,
        &["commit", "-q", "-m", "format"],
    ));
    ok(git(&home, Some(&upstream), None, &["tag", "v1"]));
    ok(git(
        &home,
        Some(&home),
        None,
        &["clone", "-q", upstream.to_str().unwrap(), "clone"],
    ));
    let clone = home.join("clone");
    let url = server.git_url("core", "api");
    let out = failed(git(
        &home,
        Some(&clone),
        Some(&bob),
        &["push", "--porcelain", "--mirror", &url],
    ));
    assert!(
        out.contains(
            "remote: potluck-reject safety_relaxation refs/remotes/origin/main / ref_namespace"
        ),
        "{out}"
    );
    // A refused push records nothing, so give a stray record time to land before looking.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        import_pending(&server).await,
        "a refused import stays pending"
    );
    ok(git(
        &home,
        Some(&clone),
        Some(&bob),
        &[
            "push",
            "-q",
            &url,
            "refs/heads/*:refs/heads/*",
            "refs/tags/*:refs/tags/*",
        ],
    ));
    // The record lands after git exits, so poll the row until the import state is gone.
    timeout(Duration::from_secs(10), async {
        while import_pending(&server).await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the heads and tags import ends the import within 10 s");
}
