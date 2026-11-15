//! `Actor::User`, `AuthUser`, the hasher on `AppState`, and the test helpers that build them.

use potluck::api::{Actor, AuthUser};
use potluck::auth::password::Passwords;
use potluck::auth::token::digest;

use crate::common::{create_user, fast_passwords, mint_token, test_state};

#[tokio::test]
async fn a_user_actor_exposes_its_user_and_admin_flag() {
    let (_dir, db, _state) = test_state().await;
    let bob = create_user(&db, "bob", "pw", false).await;
    let alice = create_user(&db, "alice", "pw", true).await;
    let as_bob = Actor::User(AuthUser::new(bob.clone(), 7));
    let as_alice = Actor::User(AuthUser::new(alice, 8));
    assert_eq!(as_bob.user().map(|u| u.user().username()), Some("bob"));
    assert_eq!(as_bob.user().map(AuthUser::token_id), Some(7));
    assert!(!as_bob.is_server_admin());
    assert!(as_alice.is_server_admin());
    assert!(Actor::System.is_server_admin(), "system acts as an admin");
    assert!(Actor::System.user().is_none() && Actor::Anonymous.user().is_none());
    assert!(!Actor::Anonymous.is_server_admin());
    assert_eq!(
        as_bob,
        Actor::User(AuthUser::new(bob, 7)),
        "actors compare by value"
    );
}

#[tokio::test]
async fn a_user_actor_s_debug_redacts_the_password_hash() {
    let (_dir, db, _state) = test_state().await;
    let bob = create_user(&db, "bob", "pw-123456", false).await;
    let hash = bob.password_hash().unwrap().to_owned();
    let shown = format!("{:?}", Actor::User(AuthUser::new(bob, 1)));
    assert!(shown.contains("bob"), "{shown}");
    assert!(
        !shown.contains(&hash[hash.len() - 16..]),
        "the hash leaked: {shown}"
    );
}

#[tokio::test]
async fn create_user_hashes_at_test_cost_and_mint_token_stores_the_digest() {
    let (_dir, db, state) = test_state().await;
    let bob = create_user(&db, "bob", "pw-123456", true).await;
    assert!(bob.server_admin());
    assert_eq!(bob.display_name(), "bob");
    let phc = bob.password_hash().unwrap();
    assert!(phc.starts_with("$argon2id$v=19$m=1024,t=1,p=1$"), "{phc}");
    assert!(
        state.passwords().verify("pw-123456", phc).unwrap(),
        "the state's hasher is the fast one"
    );
    assert_eq!(state.passwords(), &fast_passwords());
    let secret = mint_token(&db, &bob, "laptop", Some("2030-01-01T00:00:00Z")).await;
    let candidates = db.tokens_by_prefix(&secret[..8]).await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].digest(), &digest(&secret));
    assert_eq!(candidates[0].token().name(), "laptop");
    assert_eq!(
        candidates[0].token().expires_at(),
        Some("2030-01-01T00:00:00Z")
    );
}

#[tokio::test]
async fn app_state_defaults_to_the_production_cost() {
    let (dir, db) = crate::common::migrated_db().await;
    let state = potluck::api::AppState::new(db, crate::common::config_for(dir.path()));
    assert_eq!(state.passwords(), &Passwords::new());
}
