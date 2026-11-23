//! A real push through the engine ends in confirmed moves on the workspace's topic.

use std::time::Duration;

use potluck::git::access::PushContext;
use potluck::git::push::{MAX_COMMANDS, RefMove, ZERO_SHA};
use potluck::notices::Subscription;
use potluck::role::Role;
use potluck::slug::Slug;
use tokio::time::timeout;

use crate::common::{
    GitServer, TestAccess, client, failed, git, git_server, ok, work_tree, write_hook,
};

const TOKEN: &str = "plk_writer_token_000000000000000000000000000";
const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// A server on `core/api` with `TOKEN` as a writer; named so no test's local shadows it.
async fn start_server(ctx: Option<PushContext>) -> GitServer {
    let mut access = TestAccess::new()
        .workspace("core", "api")
        .grant(TOKEN, "core", "api", Role::Write)
        .hook_env("POTLUCK_DEFAULT_BRANCH", "main");
    if let Some(ctx) = ctx {
        access = access.push_context(ctx);
    }
    let server = git_server(access, None, &[]).await;
    server
        .repos
        .init(
            &Slug::parse("core").unwrap(),
            &Slug::parse("api").unwrap(),
            "main",
        )
        .await
        .unwrap();
    server
}

async fn take(sub: &mut Subscription, n: usize) -> Vec<RefMove> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(
            timeout(Duration::from_secs(10), sub.next())
                .await
                .expect("a move within 10 s")
                .expect("stream open"),
        );
    }
    out.sort_by(|a, b| a.ref_name().cmp(b.ref_name()));
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn an_accepted_push_publishes_one_confirmed_move_per_ref() {
    let server = start_server(Some(PushContext::new(7, "bob"))).await;
    let mut sub = server.notices.subscribe(7, 1, 1);
    let mut elsewhere = server.notices.subscribe(8, 1, 1);
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("core", "api");
    let work = work_tree(&home, "work");
    let tip = ok(git(&home, Some(&work), None, &["rev-parse", "HEAD"]))
        .trim()
        .to_owned();
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main", "main:refs/heads/topic"],
    ));
    assert_eq!(
        take(&mut sub, 2).await,
        vec![
            RefMove::new("refs/heads/main", ZERO_SHA, &tip),
            RefMove::new("refs/heads/topic", ZERO_SHA, &tip)
        ]
    );
    std::fs::write(work.join("README"), "two\n").unwrap();
    ok(git(
        &home,
        Some(&work),
        None,
        &["commit", "-q", "-am", "two"],
    ));
    let tip2 = ok(git(&home, Some(&work), None, &["rev-parse", "HEAD"]))
        .trim()
        .to_owned();
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &url, "main"],
    ));
    assert_eq!(
        take(&mut sub, 1).await,
        vec![RefMove::new("refs/heads/main", &tip, &tip2)]
    );
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", "--delete", &url, "topic"],
    ));
    assert_eq!(
        take(&mut sub, 1).await,
        vec![RefMove::new("refs/heads/topic", &tip, ZERO_SHA)]
    );
    assert!(
        timeout(Duration::from_millis(200), elsewhere.next())
            .await
            .is_err(),
        "another workspace heard nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declined_push_and_a_push_without_context_publish_nothing() {
    let server = start_server(Some(PushContext::new(7, "bob"))).await;
    let mut sub = server.notices.subscribe(7, 1, 1);
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let url = server.url("core", "api");
    let work = work_tree(&home, "work");
    write_hook(&server.dir.path().join("hooks"), "exit 1");
    let out = failed(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "--porcelain", &url, "main"],
    ));
    assert!(out.contains("pre-receive hook declined"), "{out}");
    assert!(
        timeout(Duration::from_millis(300), sub.next())
            .await
            .is_err(),
        "nothing moved, nothing published"
    );
    let plain = start_server(None).await;
    let mut any = plain.notices.subscribe(7, 1, 1);
    let home = plain.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let work = work_tree(&home, "work");
    ok(git(
        &home,
        Some(&work),
        Some(TOKEN),
        &["push", "-q", &plain.url("core", "api"), "main"],
    ));
    assert!(
        timeout(Duration::from_millis(300), any.next())
            .await
            .is_err(),
        "no context, no record"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_list_over_the_cap_is_refused_before_git_runs() {
    let server = start_server(Some(PushContext::new(7, "bob"))).await;
    let mut sub = server.notices.subscribe(7, 1, 1);
    let mut body = Vec::new();
    for i in 0..=MAX_COMMANDS {
        let line = format!("{ZERO_SHA} {A} refs/heads/b{i}\n");
        body.extend_from_slice(format!("{:04x}", line.len() + 4).as_bytes());
        body.extend_from_slice(line.as_bytes());
    }
    body.extend_from_slice(b"0000");
    let r = client()
        .post(format!("{}/git-receive-pack", server.url("core", "api")))
        .bearer_auth(TOKEN)
        .header("content-type", "application/x-git-receive-pack-request")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "invalid_request", "{v}");
    assert!(
        v["error"]["field"].is_null(),
        "a request-shape fault names no body field: {v}"
    );
    assert!(
        timeout(Duration::from_millis(300), sub.next())
            .await
            .is_err(),
        "git never ran, nothing was published"
    );
}
