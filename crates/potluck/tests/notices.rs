//! The notice registry: targeted closes, fan-out, and bounded lag.

use std::time::Duration;

use potluck::git::push::{RefMove, ZERO_SHA};
use potluck::notices::{CAPACITY, Notices, Subscription};
use tokio::time::timeout;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn mv(name: &str) -> RefMove {
    RefMove::new(name, ZERO_SHA, A)
}

async fn ended(sub: &mut Subscription) -> bool {
    matches!(timeout(Duration::from_secs(2), sub.next()).await, Ok(None))
}

async fn silent(sub: &mut Subscription) -> bool {
    timeout(Duration::from_millis(100), sub.next())
        .await
        .is_err()
}

#[tokio::test]
async fn publish_reaches_every_stream_of_that_workspace_only() {
    let n = Notices::new();
    let mut a = n.subscribe(1, 10, 100);
    let mut b = n.subscribe(1, 11, 110);
    let mut c = n.subscribe(2, 10, 100);
    n.publish(1, &[mv("refs/heads/main"), mv("refs/heads/topic")]);
    for s in [&mut a, &mut b] {
        assert_eq!(s.next().await, Some(mv("refs/heads/main")));
        assert_eq!(s.next().await, Some(mv("refs/heads/topic")));
    }
    assert!(silent(&mut c).await, "workspace 2 heard nothing");
    n.publish(3, &[mv("refs/heads/main")]);
    assert_eq!(
        n.open(),
        3,
        "publishing to a workspace without streams opens none"
    );
}

#[tokio::test]
async fn closes_are_exact() {
    let n = Notices::new();
    let mut t1 = n.subscribe(1, 10, 100);
    let mut t2 = n.subscribe(1, 10, 101);
    let mut other_ws = n.subscribe(2, 10, 100);
    let mut other_user = n.subscribe(1, 11, 110);
    assert_eq!(n.workspaces_of(10), vec![1, 2]);
    assert_eq!(n.close_token(100), 2, "token 100 opened two streams");
    assert!(ended(&mut t1).await && ended(&mut other_ws).await);
    n.publish(1, &[mv("refs/heads/main")]);
    assert_eq!(
        t2.next().await,
        Some(mv("refs/heads/main")),
        "token 101 lives on"
    );
    assert_eq!(other_user.next().await, Some(mv("refs/heads/main")));
    let mut again = n.subscribe(2, 10, 101);
    assert_eq!(n.close_grant(1, 10), 1);
    assert!(ended(&mut t2).await);
    assert!(
        silent(&mut again).await,
        "the grant on workspace 1 does not touch workspace 2"
    );
    assert_eq!(n.close_user(10), 1);
    assert!(ended(&mut again).await);
    let mut ws1 = n.subscribe(1, 12, 120);
    assert_eq!(n.close_workspace(1), 2);
    assert!(ended(&mut other_user).await && ended(&mut ws1).await);
    let mut last = n.subscribe(5, 50, 500);
    assert_eq!(n.close_all(), 1);
    assert!(ended(&mut last).await);
    assert_eq!(n.open(), 0);
    assert_eq!(n.close_token(999), 0);
    let mut late = n.subscribe(5, 50, 500);
    assert!(
        ended(&mut late).await,
        "after close_all a new stream ends at once"
    );
    assert_eq!(n.open(), 0, "and is not registered");
}

#[tokio::test]
async fn dropping_unregisters_and_a_lagged_stream_ends() {
    let n = Notices::new();
    let a = n.subscribe(1, 10, 100);
    let mut b = n.subscribe(1, 10, 101);
    assert_eq!(n.open(), 2);
    drop(a);
    assert_eq!(
        n.open(),
        1,
        "a dropped subscription is gone from the registry"
    );
    assert_eq!(n.workspaces_of(10), vec![1]);
    let moves: Vec<RefMove> = (0..=CAPACITY)
        .map(|i| mv(&format!("refs/heads/b{i}")))
        .collect();
    n.publish(1, &moves);
    assert!(
        ended(&mut b).await,
        "one more than CAPACITY unread moves ends the stream"
    );
    let clone = n.clone();
    let mut c = clone.subscribe(1, 10, 102);
    drop(n);
    drop(clone);
    assert!(
        ended(&mut c).await,
        "a registry that is gone ends its streams"
    );
}

#[tokio::test]
async fn a_closed_stream_stays_ended() {
    let n = Notices::new();
    let mut s = n.subscribe(1, 10, 100);
    n.close_token(100);
    assert!(ended(&mut s).await);
    assert!(
        ended(&mut s).await,
        "a second call after a close is None too"
    );
}

#[tokio::test]
async fn a_stream_opened_after_close_all_stays_ended() {
    let n = Notices::new();
    n.close_all();
    let mut s = n.subscribe(1, 10, 100);
    assert!(ended(&mut s).await);
    assert!(
        ended(&mut s).await,
        "a second call after close_all is None too"
    );
}

#[tokio::test]
async fn a_lagged_stream_does_not_resume() {
    let n = Notices::new();
    let mut s = n.subscribe(1, 10, 100);
    let moves: Vec<RefMove> = (0..=CAPACITY)
        .map(|i| mv(&format!("refs/heads/b{i}")))
        .collect();
    n.publish(1, &moves);
    assert!(ended(&mut s).await);
    assert!(
        ended(&mut s).await,
        "a lagged stream does not pick up again"
    );
}

#[tokio::test]
async fn a_stream_given_a_lifetime_ends_when_it_runs_out() {
    let n = Notices::new();
    let mut lasting = n.subscribe(1, 10, 100);
    let started = tokio::time::Instant::now();
    let mut short = n
        .subscribe(1, 10, 101)
        .close_after(Duration::from_millis(300));
    n.publish(1, &[mv("refs/heads/main")]);
    assert_eq!(
        short.next().await,
        Some(mv("refs/heads/main")),
        "moves flow until then"
    );
    assert!(ended(&mut short).await, "the lifetime ends the stream");
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "not before it runs out"
    );
    assert!(ended(&mut short).await, "and it stays ended");
    n.publish(1, &[mv("refs/heads/topic")]);
    assert_eq!(lasting.next().await, Some(mv("refs/heads/main")));
    assert_eq!(
        lasting.next().await,
        Some(mv("refs/heads/topic")),
        "a stream without a lifetime keeps going"
    );
}

#[tokio::test]
async fn a_lifetime_already_spent_ends_the_stream_at_once() {
    let n = Notices::new();
    let mut sub = n.subscribe(1, 10, 100).close_after(Duration::ZERO);
    assert!(ended(&mut sub).await);
}
