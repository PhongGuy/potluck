//! The JSON objects serialise to the contract's shapes, and the test app serves.

use potluck::api::objects::{IdentityJson, TokenJson, UserJson, identity};
use potluck::db::accounts::{NewToken, NewUser, UserPatch};
use serde_json::json;

use crate::common::{client, migrated_db, test_app};

#[tokio::test]
async fn user_json_has_the_six_fields_and_no_hash() {
    let (_dir, db) = migrated_db().await;
    let bob = db
        .create_user(NewUser::new("bob", "Bob").password_hash("$argon2id$secret"))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(UserJson::from(&bob)).unwrap(),
        json!({"username": "bob", "display_name": "Bob", "email": null, "server_admin": false, "disabled": false, "created_at": bob.created_at()})
    );
}

#[tokio::test]
async fn token_json_has_the_six_fields_and_no_digest() {
    let (_dir, db) = migrated_db().await;
    let bob = db.create_user(NewUser::new("bob", "Bob")).await.unwrap();
    let t = db
        .create_token(NewToken::new(
            bob.id(),
            "ci",
            "plk_aaaa",
            [7; 32],
            Some("2030-01-01T00:00:00Z"),
        ))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(TokenJson::from(&t)).unwrap(),
        json!({"id": t.id(), "name": "ci", "prefix": "plk_aaaa", "created_at": t.created_at(), "last_used_at": null, "expires_at": "2030-01-01T00:00:00Z"})
    );
}

#[tokio::test]
async fn identity_falls_back_to_username_and_a_noreply_address() {
    let (_dir, db) = migrated_db().await;
    let bob = db.create_user(NewUser::new("bob", "")).await.unwrap();
    let id: IdentityJson = identity(&bob, "https://team.example.com/potluck");
    assert_eq!(
        serde_json::to_value(&id).unwrap(),
        json!({"name": "bob", "email": "bob@users.noreply.team.example.com"})
    );
    let bob = db
        .update_user(
            "bob",
            UserPatch::default()
                .display_name("Bob B")
                .email(Some("bob@example.com")),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(identity(&bob, "http://127.0.0.1:8080")).unwrap(),
        json!({"name": "Bob B", "email": "bob@example.com"})
    );
}

#[tokio::test]
async fn the_test_app_serves_the_api() {
    let app = test_app().await;
    let r = client().get(app.url("/health")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        app.db.list_users().await.unwrap().len(),
        0,
        "the app's database is reachable"
    );
}
