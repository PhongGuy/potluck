//! Real git against the in-process server: clone, push, fetch, and a gzipped negotiation.

use potluck::role::Role;
use potluck::slug::Slug;

use crate::common::{TestAccess, failed, git, git_server, ok, work_tree};

const TOKEN: &str = "plk_writer_token_000000000000000000000000000";

fn slug(s: &str) -> Slug {
    Slug::parse(s).unwrap()
}

async fn server_with_repo() -> crate::common::GitServer {
    let access =
        TestAccess::new()
            .workspace("core", "api")
            .grant(TOKEN, "core", "api", Role::Write);
    let server = git_server(access, None, &[]).await;
    server
        .repos
        .init(&slug("core"), &slug("api"), "main")
        .await
        .unwrap();
    server
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_push_and_fetch_round_trip() {
    let server = server_with_repo().await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("core", "api");
    let work = work_tree(&home, "work");
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
    ok(git(
        &home,
        Some(&home),
        Some(TOKEN),
        &["clone", "-q", &url, "clone"],
    ));
    assert_eq!(
        std::fs::read_to_string(home.join("clone").join("README")).unwrap(),
        "hello\n"
    );
    std::fs::write(work.join("README"), "hello again\n").unwrap();
    ok(git(
        &home,
        Some(&work),
        None,
        &["commit", "-q", "-am", "two"],
    ));
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
    let clone = home.join("clone");
    ok(git(
        &home,
        Some(&clone),
        Some(TOKEN),
        &["pull", "-q", "--ff-only", "origin", "main"],
    ));
    assert_eq!(
        std::fs::read_to_string(clone.join("README")).unwrap(),
        "hello again\n"
    );
    let refs = ok(git(&home, Some(&home), Some(TOKEN), &["ls-remote", &url]));
    assert!(refs.contains("refs/heads/main"), "{refs}");
    let anon = failed(git(
        &home,
        Some(&home),
        None,
        &["clone", "-q", &url, "anon"],
    ));
    assert!(
        anon.contains("terminal prompts disabled"),
        "no token means a 401 challenge: {anon}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fetch_with_a_large_negotiation_is_served() {
    let server = server_with_repo().await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("core", "api");
    let work = work_tree(&home, "work");
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
    ok(git(
        &home,
        Some(&home),
        Some(TOKEN),
        &["clone", "-q", &url, "clone"],
    ));
    let clone = home.join("clone");
    // 30 local branch tips become 30 `have` lines: over 1 KiB, which git 2.56 gzips.
    for i in 0..30 {
        ok(git(
            &home,
            Some(&clone),
            None,
            &["checkout", "-q", "-b", &format!("b{i}")],
        ));
        std::fs::write(clone.join(format!("f{i}")), i.to_string()).unwrap();
        ok(git(&home, Some(&clone), None, &["add", "."]));
        ok(git(
            &home,
            Some(&clone),
            None,
            &["commit", "-q", "-m", &format!("c{i}")],
        ));
    }
    std::fs::write(work.join("README"), "server side\n").unwrap();
    ok(git(
        &home,
        Some(&work),
        None,
        &["commit", "-q", "-am", "server"],
    ));
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
    ok(git(
        &home,
        Some(&clone),
        Some(TOKEN),
        &["fetch", "-q", "origin"],
    ));
    let log = ok(git(
        &home,
        Some(&clone),
        None,
        &["log", "-1", "--format=%s", "origin/main"],
    ));
    assert_eq!(log.trim(), "server");
}
