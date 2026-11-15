//! The SSE stream: what a connect sends, what a push adds, and when the server hangs up.

use std::path::PathBuf;
use std::time::Duration;

use potluck::git::push::ZERO_SHA;
use reqwest::Method;
use serde_json::json;
use tokio::time::timeout;

use crate::common::{
    FullServer, SEED_COMMIT, SseReader, api_admin, api_team, api_user, api_workspace, call,
    full_server, git, ok, ref_event, work_tree,
};

const T: Duration = Duration::from_secs(10);

pub(crate) struct Arena {
    pub server: FullServer,
    pub home: PathBuf,
    pub root: String,
    pub bob: String,
    pub ann: String,
}

/// `core/api` seeded by `root`; `bob` reads it, `ann` writes it.
pub(crate) async fn arena() -> Arena {
    let server = full_server().await;
    let root = api_admin(server.db()).await;
    let (_, bob) = api_user(server.db(), "bob").await;
    let (_, ann) = api_user(server.db(), "ann").await;
    api_team(&server.api(""), &root, "core").await;
    api_workspace(&server.api(""), &root, "core", "api", true).await;
    grant(&server, &root, "api", "user", "bob", "read").await;
    grant(&server, &root, "api", "user", "ann", "write").await;
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    Arena {
        server,
        home,
        root,
        bob,
        ann,
    }
}

pub(crate) async fn grant(
    server: &FullServer,
    root: &str,
    ws: &str,
    kind: &str,
    name: &str,
    role: &str,
) {
    let (status, body) = call(
        &server.api(&format!("/workspaces/core/{ws}/grants/{kind}/{name}")),
        Method::PUT,
        Some(root),
        Some(json!({"role": role})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

pub(crate) fn events_url(a: &Arena, ws: &str) -> String {
    a.server.api(&format!("/workspaces/core/{ws}/events"))
}

/// Pushes `refspec` from `work` as ann; returns `work`'s tip.
pub(crate) fn push(a: &Arena, work: &std::path::Path, refspec: &str) -> String {
    ok(git(
        &a.home,
        Some(work),
        Some(&a.ann),
        &["push", "-q", &a.server.git_url("core", "api"), refspec],
    ));
    ok(git(&a.home, Some(work), None, &["rev-parse", "HEAD"]))
        .trim()
        .to_owned()
}

pub(crate) async fn next(r: &mut SseReader) -> Option<Vec<String>> {
    timeout(T, r.next_block())
        .await
        .expect("a block within 10 s")
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_sends_retry_then_the_snapshot_then_live_moves() {
    let a = arena().await;
    let mut r = SseReader::open(&events_url(&a, "api"), &a.bob)
        .await
        .expect("read role streams");
    assert_eq!(next(&mut r).await.unwrap(), vec!["retry: 2000"]);
    assert_eq!(
        ref_event(&next(&mut r).await.unwrap()),
        ("refs/heads/main".to_owned(), SEED_COMMIT.to_owned()),
        "the seeded branch is the whole snapshot"
    );
    let work = work_tree(&a.home, "work");
    let tip = push(&a, &work, "main:refs/heads/topic");
    assert_eq!(
        ref_event(&next(&mut r).await.unwrap()),
        ("refs/heads/topic".to_owned(), tip.clone()),
        "a live move"
    );
    // `r` has read the push's notice, so its publish is done: a stream opened now has both
    // branches in its snapshot and nothing live, with no race against the reaper.
    let mut later = SseReader::open(&events_url(&a, "api"), &a.bob)
        .await
        .unwrap();
    assert_eq!(next(&mut later).await.unwrap(), vec!["retry: 2000"]);
    assert_eq!(
        ref_event(&next(&mut later).await.unwrap()),
        ("refs/heads/main".to_owned(), SEED_COMMIT.to_owned()),
        "branches in ref order"
    );
    assert_eq!(
        ref_event(&next(&mut later).await.unwrap()),
        ("refs/heads/topic".to_owned(), tip.clone())
    );
    assert!(
        timeout(Duration::from_millis(300), later.next_block())
            .await
            .is_err(),
        "no duplicate of the move that preceded the connect"
    );
    std::fs::write(work.join("README"), "two\n").unwrap();
    ok(git(
        &a.home,
        Some(&work),
        None,
        &["commit", "-q", "-am", "two"],
    ));
    let tip2 = push(&a, &work, "main:refs/heads/topic");
    assert_eq!(
        ref_event(&next(&mut r).await.unwrap()),
        ("refs/heads/topic".to_owned(), tip2.clone())
    );
    assert_eq!(
        ref_event(&next(&mut later).await.unwrap()),
        ("refs/heads/topic".to_owned(), tip2),
        "both streams hear it"
    );
    ok(git(
        &a.home,
        Some(&work),
        Some(&a.ann),
        &[
            "push",
            "-q",
            "--delete",
            &a.server.git_url("core", "api"),
            "topic",
        ],
    ));
    assert_eq!(
        ref_event(&next(&mut r).await.unwrap()),
        ("refs/heads/topic".to_owned(), ZERO_SHA.to_owned()),
        "a deleted branch carries the zero sha"
    );
    let (_, carol) = api_user(a.server.db(), "carol").await;
    assert_eq!(
        SseReader::open(&events_url(&a, "api"), &carol).await.err(),
        Some(404),
        "no role is not found"
    );
    assert_eq!(
        SseReader::open(&events_url(&a, "api"), "plk_unknown")
            .await
            .err(),
        Some(401)
    );
    assert_eq!(
        SseReader::open(&events_url(&a, "nope"), &a.bob).await.err(),
        Some(404)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_a_token_closes_its_streams_only_and_disabling_closes_them_all() {
    let a = arena().await;
    let bob = a
        .server
        .db()
        .user_by_username("bob")
        .await
        .unwrap()
        .unwrap();
    let bob2 = crate::common::mint_token(a.server.db(), &bob, "second", None).await;
    let mut r1 = SseReader::open(&events_url(&a, "api"), &a.bob)
        .await
        .unwrap();
    let mut r2 = SseReader::open(&events_url(&a, "api"), &bob2)
        .await
        .unwrap();
    for r in [&mut r1, &mut r2] {
        next(r).await.unwrap();
        next(r).await.unwrap();
    }
    let (_, tokens) = call(&a.server.api("/tokens"), Method::GET, Some(&a.bob), None).await;
    let first = tokens
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "laptop")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let (status, _) = call(
        &a.server.api(&format!("/tokens/{first}")),
        Method::DELETE,
        Some(&a.bob),
        None,
    )
    .await;
    assert_eq!(status, 204);
    assert_eq!(
        next(&mut r1).await,
        None,
        "the revoked token's stream is closed"
    );
    let work = work_tree(&a.home, "work");
    let tip = push(&a, &work, "main:refs/heads/topic");
    assert_eq!(
        ref_event(&next(&mut r2).await.unwrap()),
        ("refs/heads/topic".to_owned(), tip),
        "the other token still listens"
    );
    assert_eq!(
        SseReader::open(&events_url(&a, "api"), &a.bob).await.err(),
        Some(401),
        "the next connect is signed out"
    );
    let (status, _) = call(
        &a.server.api("/users/bob"),
        Method::PATCH,
        Some(&a.root),
        Some(json!({"disabled": true})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        next(&mut r2).await,
        None,
        "disabling closes the user's streams"
    );
    assert_eq!(
        SseReader::open(&events_url(&a, "api"), &bob2).await.err(),
        Some(401)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn losing_the_last_grant_closes_the_stream_and_the_next_connect_is_404() {
    let a = arena().await;
    let (status, _) = call(
        &a.server.api("/teams/core/members/bob"),
        Method::PUT,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(status, 200);
    grant(&a.server, &a.root, "api", "team", "core", "read").await;
    let mut r = SseReader::open(&events_url(&a, "api"), &a.bob)
        .await
        .unwrap();
    next(&mut r).await.unwrap();
    next(&mut r).await.unwrap();
    let (status, _) = call(
        &a.server.api("/workspaces/core/api/grants/user/bob"),
        Method::DELETE,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(status, 204);
    let work = work_tree(&a.home, "work");
    let tip = push(&a, &work, "main:refs/heads/topic");
    assert_eq!(
        ref_event(&next(&mut r).await.unwrap()),
        ("refs/heads/topic".to_owned(), tip),
        "the team grant keeps the stream open"
    );
    let (status, _) = call(
        &a.server.api("/workspaces/core/api/grants/team/core"),
        Method::DELETE,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(status, 204);
    assert_eq!(next(&mut r).await, None, "the last grant went");
    assert_eq!(
        SseReader::open(&events_url(&a, "api"), &a.bob).await.err(),
        Some(404)
    );
    grant(&a.server, &a.root, "api", "team", "core", "read").await;
    // The snapshot now holds `main` and `topic`; read it all so only the close is left.
    let mut r = SseReader::open(&events_url(&a, "api"), &a.bob)
        .await
        .unwrap();
    for _ in 0..3 {
        next(&mut r).await.unwrap();
    }
    let (status, _) = call(
        &a.server.api("/teams/core/members/bob"),
        Method::DELETE,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(status, 204);
    assert_eq!(
        next(&mut r).await,
        None,
        "leaving the team loses the team grant"
    );
    let mut ann = SseReader::open(&events_url(&a, "api"), &a.ann)
        .await
        .unwrap();
    for _ in 0..3 {
        next(&mut ann).await.unwrap();
    }
    let (status, _) = call(
        &a.server.api("/workspaces/core/api/grants/team/core"),
        Method::DELETE,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(status, 204);
    assert!(
        timeout(Duration::from_millis(300), ann.next_block())
            .await
            .is_err(),
        "ann's own grant was not touched"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_workspace_and_shutting_down_close_streams() {
    let a = arena().await;
    api_workspace(&a.server.api(""), &a.root, "core", "docs", true).await;
    grant(&a.server, &a.root, "docs", "user", "bob", "read").await;
    let mut api = SseReader::open(&events_url(&a, "api"), &a.bob)
        .await
        .unwrap();
    let mut docs = SseReader::open(&events_url(&a, "docs"), &a.bob)
        .await
        .unwrap();
    for r in [&mut api, &mut docs] {
        next(r).await.unwrap();
        next(r).await.unwrap();
    }
    let (status, _) = call(
        &a.server.api("/workspaces/core/api"),
        Method::DELETE,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(status, 204);
    assert_eq!(next(&mut api).await, None, "the workspace is gone");
    assert_eq!(
        SseReader::open(&events_url(&a, "api"), &a.bob).await.err(),
        Some(404)
    );
    let Arena { server, .. } = a;
    let FullServer { dir, running } = server;
    timeout(Duration::from_secs(5), running.shutdown())
        .await
        .expect("shutdown is not held up by the open stream");
    assert_eq!(
        timeout(T, docs.next_block())
            .await
            .expect("the stream ends"),
        None,
        "shutdown closed it"
    );
    drop(dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_admin_flag_and_a_failed_repository_removal_still_close_streams() {
    let a = arena().await;
    let boss = crate::common::create_user(a.server.db(), "boss", "pw-123456", true).await;
    let boss = crate::common::mint_token(a.server.db(), &boss, "boss", None).await;
    let mut r = SseReader::open(&events_url(&a, "api"), &a.root)
        .await
        .expect("a server admin reads without a grant");
    next(&mut r).await.unwrap();
    next(&mut r).await.unwrap();
    let (status, _) = call(
        &a.server.api("/users/root"),
        Method::PATCH,
        Some(&boss),
        Some(json!({"server_admin": false})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        next(&mut r).await,
        None,
        "no grant of their own: the stream went with the flag"
    );
    assert_eq!(
        SseReader::open(&events_url(&a, "api"), &a.root).await.err(),
        Some(404)
    );
    api_workspace(&a.server.api(""), &boss, "core", "docs", true).await;
    grant(&a.server, &boss, "docs", "user", "bob", "read").await;
    let mut docs = SseReader::open(&events_url(&a, "docs"), &a.bob)
        .await
        .unwrap();
    next(&mut docs).await.unwrap();
    next(&mut docs).await.unwrap();
    // A plain file where the repository directory was makes `remove_dir_all` fail, so the
    // deletion removes the rows and then answers 500.
    let repo = crate::common::repo_path(a.server.dir.path(), "core", "docs");
    std::fs::remove_dir_all(&repo).unwrap();
    std::fs::write(&repo, "not a directory").unwrap();
    let (status, body) = call(
        &a.server.api("/workspaces/core/docs"),
        Method::DELETE,
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (500, Some("internal")),
        "{body}"
    );
    assert_eq!(
        next(&mut docs).await,
        None,
        "the rows are gone, so the streams are too"
    );
    assert_eq!(
        SseReader::open(&events_url(&a, "docs"), &a.bob).await.err(),
        Some(404)
    );
}
