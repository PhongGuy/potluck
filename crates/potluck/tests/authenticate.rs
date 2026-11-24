//! `authenticate`: the token verifier, every refusal, the touch, and no token in the log.

use potluck::api::auth::authenticate;
use potluck::api::credential::Credential;
use potluck::api::error::ErrorCode;
use potluck::clock::{now_rfc3339, shift_secs};
use potluck::db::accounts::{NewToken, TokenOrigin, UserPatch};
use potluck::db::audit::AuditCtx;

use crate::common::{create_user, mint_token, test_state};
use crate::logs::captured_logs;

fn cred(token: &str) -> Credential {
    Credential::new(token.to_owned())
}

#[tokio::test]
async fn a_valid_token_resolves_to_its_user_and_touches_last_used() {
    let (_dir, db, state) = test_state().await;
    let bob = create_user(&db, "bob", "pw", false).await;
    let secret = mint_token(&db, &bob, "laptop", None).await;
    let auth = authenticate(&state, &cred(&secret)).await.unwrap();
    assert_eq!(auth.user().username(), "bob");
    let tokens = db.tokens_for_user(bob.id()).await.unwrap();
    assert_eq!(auth.token_id(), tokens[0].id());
    assert!(tokens[0].last_used_at().is_some(), "touched on use");
}

#[tokio::test]
async fn candidates_sharing_a_prefix_resolve_to_the_right_one() {
    let (_dir, db, state) = test_state().await;
    let bob = create_user(&db, "bob", "pw", false).await;
    let alice = create_user(&db, "alice", "pw", false).await;
    let secret = mint_token(&db, &bob, "real", None).await;
    // Three decoys with bob's prefix but other digests, owned by alice.
    for i in 1u8..=3 {
        db.create_token(
            NewToken::new(alice.id(), "decoy", &secret[..8], [i; 32], None),
            TokenOrigin::Api,
            &AuditCtx::system(),
        )
        .await
        .unwrap();
    }
    let auth = authenticate(&state, &cred(&secret)).await.unwrap();
    assert_eq!(auth.user().username(), "bob");
    assert_eq!(db.tokens_by_prefix(&secret[..8]).await.unwrap().len(), 4);
}

#[tokio::test]
async fn unknown_expired_revoked_malformed_and_disabled_are_401() {
    let (_dir, db, state) = test_state().await;
    let bob = create_user(&db, "bob", "pw", false).await;
    let good = mint_token(&db, &bob, "ok", None).await;
    let expired = mint_token(&db, &bob, "old", Some("2020-01-01T00:00:00Z")).await;
    let revoked = mint_token(&db, &bob, "gone", None).await;
    let revoked_id = db.tokens_for_user(bob.id()).await.unwrap()[2].id();
    assert!(
        db.delete_token(revoked_id, &AuditCtx::system())
            .await
            .unwrap()
    );
    let mut unknown = good.clone();
    unknown.replace_range(40.., "zzzzzzz");
    let mut messages = std::collections::HashSet::new();
    for (what, token) in [
        ("unknown", unknown.as_str()),
        ("expired", expired.as_str()),
        ("revoked", revoked.as_str()),
        ("malformed", "nope"),
        ("empty", ""),
        (
            "wrong-prefix",
            "ghp_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        ),
    ] {
        let err = authenticate(&state, &cred(token)).await.unwrap_err();
        assert_eq!(err.status(), 401, "{what}: {err:?}");
        assert_eq!(err.code(), ErrorCode::Unauthenticated, "{what}");
        let shown = format!("{err:?}");
        assert!(
            token.len() < 8 || !shown.contains(&token[8..]),
            "{what}: the token leaked: {shown}"
        );
        messages.insert(shown);
    }
    assert_eq!(
        messages.len(),
        1,
        "one error for every refusal: {messages:?}"
    );
    assert!(
        authenticate(&state, &cred(&good)).await.is_ok(),
        "the good token still works"
    );
    db.update_user(
        "bob",
        UserPatch::default().disabled(true),
        &AuditCtx::system(),
    )
    .await
    .unwrap();
    let err = authenticate(&state, &cred(&good)).await.unwrap_err();
    assert_eq!(err.status(), 401, "a disabled user's tokens stop");
}

#[tokio::test]
async fn a_disabled_server_admin_is_refused_before_becoming_a_user() {
    let (_dir, db, state) = test_state().await;
    let root = create_user(&db, "root", "pw", true).await;
    let secret = mint_token(&db, &root, "t", None).await;
    assert!(
        authenticate(&state, &cred(&secret))
            .await
            .unwrap()
            .user()
            .server_admin()
    );
    db.update_user(
        "root",
        UserPatch::default().disabled(true),
        &AuditCtx::system(),
    )
    .await
    .unwrap();
    let err = authenticate(&state, &cred(&secret)).await.unwrap_err();
    assert_eq!(
        err.status(),
        401,
        "a disabled admin keeps no admin power through a token"
    );
    assert_eq!(err.code(), ErrorCode::Unauthenticated);
}

#[tokio::test]
async fn expiry_is_inclusive_at_the_boundary() {
    let (_dir, db, state) = test_state().await;
    let bob = create_user(&db, "bob", "pw", false).await;
    let now = now_rfc3339();
    let at_now = mint_token(&db, &bob, "now", Some(&now)).await;
    assert_eq!(
        authenticate(&state, &cred(&at_now))
            .await
            .unwrap_err()
            .status(),
        401,
        "expires_at == now is expired"
    );
    let soon = mint_token(&db, &bob, "soon", Some(&shift_secs(&now, 3600).unwrap())).await;
    assert!(
        authenticate(&state, &cred(&soon)).await.is_ok(),
        "an hour away is fine"
    );
}

#[test]
fn a_failed_auth_never_logs_the_token() {
    let mut tokens = None;
    let text = captured_logs(|| {
        // A current-thread runtime runs everything on this thread, whose events the sink
        // captures; the database thread logs nothing of its own.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokens = Some(rt.block_on(async {
            let (_dir, db, state) = test_state().await;
            let bob = create_user(&db, "bob", "pw", false).await;
            let good = mint_token(&db, &bob, "t", None).await;
            let mut wrong = good.clone();
            wrong.replace_range(40.., "zzzzzzz");
            let _ = authenticate(&state, &cred(&wrong)).await;
            let _ = authenticate(&state, &cred("nope")).await;
            let _ = authenticate(&state, &cred(&good)).await;
            (good, wrong)
        }));
    });
    let (good, wrong) = tokens.expect("the runtime ran");
    assert!(
        text.contains("token rejected"),
        "the refusal was logged at debug: {text}"
    );
    assert!(text.contains(&wrong[..8]), "the prefix is logged: {text}");
    assert!(
        !text.contains(&wrong[8..]),
        "the wrong token leaked: {text}"
    );
    assert!(!text.contains(&good[8..]), "the good token leaked: {text}");
}
