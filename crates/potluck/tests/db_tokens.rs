//! The token queries on the one database thread.

use potluck::db::Db;
use potluck::db::accounts::{NewToken, NewUser};

use crate::common::migrated_db;

fn digest(n: u8) -> [u8; 32] {
    [n; 32]
}

async fn last_used(db: &Db, id: i64) -> Option<String> {
    db.token_by_id(id)
        .await
        .unwrap()
        .unwrap()
        .last_used_at()
        .map(str::to_owned)
}

#[tokio::test]
async fn tokens_round_trip_by_prefix_user_and_id() {
    let (_dir, db) = migrated_db().await;
    let bob = db.create_user(NewUser::new("bob", "Bob")).await.unwrap();
    let t1 = db
        .create_token(NewToken::new(
            bob.id(),
            "laptop",
            "plk_aaaa",
            digest(1),
            None,
        ))
        .await
        .unwrap();
    let t2 = db
        .create_token(NewToken::new(
            bob.id(),
            "ci",
            "plk_aaaa",
            digest(2),
            Some("2030-01-01T00:00:00Z"),
        ))
        .await
        .unwrap();
    assert_eq!(t1.name(), "laptop");
    assert_eq!(t1.prefix(), "plk_aaaa");
    assert_eq!(t1.user_id(), bob.id());
    assert_eq!(t1.last_used_at(), None);
    assert_eq!(t1.expires_at(), None);
    assert_eq!(t1.created_at().len(), 20);
    assert_eq!(t2.expires_at(), Some("2030-01-01T00:00:00Z"));
    let candidates = db.tokens_by_prefix("plk_aaaa").await.unwrap();
    assert_eq!(candidates.len(), 2, "both share the prefix");
    let c1 = candidates
        .iter()
        .find(|c| c.token().id() == t1.id())
        .unwrap();
    assert_eq!(c1.digest(), &digest(1));
    assert_eq!(c1.user().username(), "bob");
    assert!(db.tokens_by_prefix("plk_zzzz").await.unwrap().is_empty());
    let ids: Vec<i64> = db
        .tokens_for_user(bob.id())
        .await
        .unwrap()
        .iter()
        .map(|t| t.id())
        .collect();
    assert_eq!(ids, [t1.id(), t2.id()], "creation order");
    assert_eq!(db.token_by_id(t2.id()).await.unwrap().unwrap().name(), "ci");
    assert!(db.token_by_id(9999).await.unwrap().is_none());
}

#[tokio::test]
async fn delete_token_reports_whether_a_row_went() {
    let (_dir, db) = migrated_db().await;
    let bob = db.create_user(NewUser::new("bob", "Bob")).await.unwrap();
    let t = db
        .create_token(NewToken::new(bob.id(), "x", "plk_aaaa", digest(3), None))
        .await
        .unwrap();
    assert!(db.delete_token(t.id()).await.unwrap());
    assert!(!db.delete_token(t.id()).await.unwrap());
    assert!(db.tokens_for_user(bob.id()).await.unwrap().is_empty());
    db.touch_token(t.id(), "2026-11-15T10:00:00Z", "2026-11-15T09:59:00Z")
        .await
        .expect("touching a revoked token is not an error");
}

#[tokio::test]
async fn touch_token_writes_only_when_stale() {
    let (_dir, db) = migrated_db().await;
    let bob = db.create_user(NewUser::new("bob", "Bob")).await.unwrap();
    let t = db
        .create_token(NewToken::new(bob.id(), "x", "plk_aaaa", digest(4), None))
        .await
        .unwrap();
    let id = t.id();
    db.touch_token(id, "2026-11-15T10:00:00Z", "2026-11-15T09:59:00Z")
        .await
        .unwrap();
    assert_eq!(
        last_used(&db, id).await.as_deref(),
        Some("2026-11-15T10:00:00Z"),
        "null is written"
    );
    db.touch_token(id, "2026-11-15T10:00:30Z", "2026-11-15T09:59:30Z")
        .await
        .unwrap();
    assert_eq!(
        last_used(&db, id).await.as_deref(),
        Some("2026-11-15T10:00:00Z"),
        "inside the window: untouched"
    );
    db.touch_token(id, "2026-11-15T10:02:00Z", "2026-11-15T10:01:00Z")
        .await
        .unwrap();
    assert_eq!(
        last_used(&db, id).await.as_deref(),
        Some("2026-11-15T10:02:00Z"),
        "stale: written"
    );
}

#[tokio::test]
async fn debug_redacts_digests() {
    let (_dir, db) = migrated_db().await;
    let bob = db
        .create_user(NewUser::new("bob", "Bob").password_hash("$argon2id$hashhash"))
        .await
        .unwrap();
    let new_token = NewToken::new(bob.id(), "x", "plk_aaaa", [0xAB; 32], None);
    let shown = format!("{new_token:?}");
    assert!(
        shown.contains("plk_aaaa") && shown.contains("redacted"),
        "{shown}"
    );
    assert!(
        !shown.contains("171, 171") && !shown.contains("0xab"),
        "NewToken leaked: {shown}"
    );
    db.create_token(new_token).await.unwrap();
    let candidates = db.tokens_by_prefix("plk_aaaa").await.unwrap();
    let shown = format!("{:?}", candidates[0]);
    assert!(
        shown.contains("plk_aaaa") && shown.contains("bob"),
        "{shown}"
    );
    assert!(
        !shown.contains("171, 171") && !shown.contains("hashhash"),
        "TokenCandidate leaked: {shown}"
    );
}
