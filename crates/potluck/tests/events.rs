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
