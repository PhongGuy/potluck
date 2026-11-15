//! The user queries on the one database thread.

use potluck::api::error::{ApiError, ErrorCode};
use potluck::db::DbError;
use potluck::db::accounts::{NewUser, UserPatch};

use crate::common::migrated_db;

#[tokio::test]
async fn create_user_stores_every_column_and_lists_sorted() {
    let (_dir, db) = migrated_db().await;
    let bob = db
        .create_user(NewUser::new("bob", "Bob").password_hash("$argon2id$bob"))
        .await
        .unwrap();
    let alice = db
        .create_user(
            NewUser::new("alice", "Alice")
                .email("alice@example.com")
                .server_admin(true),
        )
        .await
        .unwrap();
    assert_eq!(bob.username(), "bob");
    assert_eq!(bob.display_name(), "Bob");
    assert_eq!(bob.email(), None);
    assert_eq!(bob.password_hash(), Some("$argon2id$bob"));
    assert!(!bob.server_admin() && !bob.disabled());
    assert_eq!(bob.created_at().len(), 20, "{}", bob.created_at());
    assert_eq!(alice.email(), Some("alice@example.com"));
    assert!(alice.server_admin());
    assert_eq!(alice.password_hash(), None);
    assert_ne!(alice.id(), bob.id());
    let names: Vec<String> = db
        .list_users()
        .await
        .unwrap()
        .iter()
        .map(|u| u.username().to_owned())
        .collect();
    assert_eq!(names, ["alice", "bob"]);
}

#[tokio::test]
async fn a_taken_username_is_a_conflict_that_maps_to_409() {
    let (_dir, db) = migrated_db().await;
    db.create_user(NewUser::new("bob", "Bob")).await.unwrap();
    let err = db
        .create_user(NewUser::new("bob", "Robert"))
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Conflict { .. }), "got {err:?}");
    assert!(err.to_string().contains("bob"), "{err}");
    assert_eq!(db.list_users().await.unwrap().len(), 1);
    let api = ApiError::from(err);
    assert_eq!(api.status(), 409);
    assert_eq!(api.code(), ErrorCode::Conflict);
}

#[tokio::test]
async fn user_by_username_is_exact() {
    let (_dir, db) = migrated_db().await;
    db.create_user(NewUser::new("bob", "Bob")).await.unwrap();
    assert_eq!(
        db.user_by_username("bob")
            .await
            .unwrap()
            .unwrap()
            .username(),
        "bob"
    );
    assert!(db.user_by_username("alice").await.unwrap().is_none());
    assert!(db.user_by_username("BOB").await.unwrap().is_none());
}

#[tokio::test]
async fn update_user_applies_only_the_set_fields() {
    let (_dir, db) = migrated_db().await;
    let bob = db
        .create_user(
            NewUser::new("bob", "Bob")
                .email("b@example.com")
                .password_hash("h1"),
        )
        .await
        .unwrap();
    let after = db
        .update_user(
            "bob",
            UserPatch::default().display_name("Robert").disabled(true),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.display_name(), "Robert");
    assert!(after.disabled());
    assert_eq!(after.email(), Some("b@example.com"), "untouched");
    assert_eq!(after.password_hash(), Some("h1"), "untouched");
    assert_eq!(after.created_at(), bob.created_at());
    let cleared = db
        .update_user(
            "bob",
            UserPatch::default()
                .email(None)
                .server_admin(true)
                .password_hash("h2"),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cleared.email(), None, "email set to null");
    assert!(cleared.server_admin());
    assert_eq!(cleared.password_hash(), Some("h2"));
    let unchanged = db
        .update_user("bob", UserPatch::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged, cleared, "an empty patch is a no-op");
    assert!(
        db.update_user("nobody", UserPatch::default().disabled(true))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn debug_redacts_the_password_hash_everywhere() {
    let phc = "$argon2id$v=19$m=1024,t=1,p=1$saltsalt$hashhash";
    let new_user = NewUser::new("bob", "Bob").password_hash(phc);
    let shown = format!("{new_user:?}");
    assert!(
        shown.contains("bob") && shown.contains("redacted"),
        "{shown}"
    );
    assert!(
        !shown.contains("hashhash") && !shown.contains("saltsalt"),
        "NewUser leaked: {shown}"
    );
    let patch = UserPatch::default()
        .display_name("Robert")
        .password_hash(phc);
    let shown = format!("{patch:?}");
    assert!(
        shown.contains("Robert") && shown.contains("redacted"),
        "{shown}"
    );
    assert!(!shown.contains("hashhash"), "UserPatch leaked: {shown}");
    let (_dir, db) = migrated_db().await;
    let bob = db.create_user(new_user).await.unwrap();
    let shown = format!("{bob:?}");
    assert!(shown.contains("bob"), "{shown}");
    assert!(
        !shown.contains("hashhash") && !shown.contains("saltsalt"),
        "UserRecord leaked: {shown}"
    );
}
