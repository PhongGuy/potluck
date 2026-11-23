//! The JSON objects serialise to the contract's shapes, and the test app serves.

use potluck::api::objects::{
    GrantJson, IdentityJson, SubjectJson, TeamJson, TokenJson, UserJson, WorkspaceJson, identity,
    noreply_host,
};
use potluck::db::accounts::{NewToken, NewUser, TokenOrigin, UserPatch};
use potluck::db::audit::AuditCtx;
use potluck::db::grants::{GrantRecord, Subject};
use potluck::db::workspaces::NewWorkspace;
use potluck::role::Role;
use potluck::slug::Slug;
use serde_json::json;

use crate::common::{client, migrated_db, test_app};

#[tokio::test]
async fn user_json_has_the_six_fields_and_no_hash() {
    let (_dir, db) = migrated_db().await;
    let bob = db
        .create_user(
            NewUser::new("bob", "Bob").password_hash("$argon2id$secret"),
            &AuditCtx::system(),
        )
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
    let bob = db
        .create_user(NewUser::new("bob", "Bob"), &AuditCtx::system())
        .await
        .unwrap();
    let t = db
        .create_token(
            NewToken::new(
                bob.id(),
                "ci",
                "plk_aaaa",
                [7; 32],
                Some("2030-01-01T00:00:00Z"),
            ),
            TokenOrigin::Api,
            &AuditCtx::system(),
        )
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
    let bob = db
        .create_user(NewUser::new("bob", ""), &AuditCtx::system())
        .await
        .unwrap();
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
            &AuditCtx::system(),
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

#[tokio::test]
async fn team_workspace_and_grant_json_match_the_contract() {
    let (_dir, db) = migrated_db().await;
    let core = db
        .create_team(&Slug::parse("core").unwrap(), "Core", &AuditCtx::system())
        .await
        .unwrap();
    let ws = db
        .create_workspace(
            NewWorkspace::new(
                core.id(),
                &Slug::parse("api").unwrap(),
                "API",
                "apinata",
                "main",
            ),
            &AuditCtx::system(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(TeamJson::new(&core, vec!["ann".into(), "bob".into()])).unwrap(),
        json!({"slug": "core", "name": "Core", "members": ["ann", "bob"]})
    );
    assert_eq!(
        serde_json::to_value(WorkspaceJson::new(
            &ws,
            Role::Write,
            "https://team.example.com/potluck"
        ))
        .unwrap(),
        json!({"team": "core", "slug": "api", "name": "API", "kind": "apinata", "default_branch": "main", "role": "write",
               "git_url": "https://team.example.com/potluck/git/core/api.git",
               "events_url": "https://team.example.com/potluck/api/v1/workspaces/core/api/events"})
    );
    assert_eq!(
        serde_json::to_value(SubjectJson::from(&Subject::User("bob".into()))).unwrap(),
        json!({"kind": "user", "name": "bob"})
    );
    assert_eq!(
        serde_json::to_value(GrantJson::from(&GrantRecord::new(
            Subject::Team("core".into()),
            Role::Admin
        )))
        .unwrap(),
        json!({"subject": {"kind": "team", "name": "core"}, "role": "admin"})
    );
    assert_eq!(
        noreply_host("https://team.example.com/potluck"),
        "team.example.com"
    );
    assert_eq!(
        noreply_host("not a url"),
        "localhost",
        "the fallback host, never a panic"
    );
}
