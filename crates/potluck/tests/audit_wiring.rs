//! Each mutating call writes exactly one audit row in its own transaction, or none.

use std::path::Path;

use potluck::db::Db;
use potluck::db::audit::{AuditCtx, AuditFilter, AuditRecord};
use reqwest::Method;
use serde_json::{Value, json};

use crate::common::{TestApp, api_admin, api_team, api_user, api_workspace, call, test_app};

/// The rows of `action`, oldest first.
async fn rows(db: &Db, action: &str) -> Vec<AuditRecord> {
    let mut rows = db
        .audit(AuditFilter::new().action(action).limit(1000))
        .await
        .unwrap();
    rows.reverse();
    rows
}

fn detail(row: &AuditRecord) -> Value {
    serde_json::from_str(row.detail().expect("a detail")).unwrap()
}

/// From here on every audit insert in `<data>/server.db` fails.
fn boom(data: &Path) {
    let c = rusqlite::Connection::open(data.join("server.db")).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    c.execute_batch(
        "CREATE TRIGGER boom BEFORE INSERT ON audit BEGIN SELECT RAISE(ABORT, 'boom'); END",
    )
    .unwrap();
}

#[tokio::test]
async fn creating_and_patching_a_user_writes_one_row_each() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let root_id = app.db.user_by_username("root").await.unwrap().unwrap().id();
    let (status, _) = call(
        &app.url("/users"),
        Method::POST,
        Some(&root),
        Some(json!({"username": "bob", "display_name": "Bob", "password": "pw-123456"})),
    )
    .await;
    assert_eq!(status, 201);
    let created = rows(&app.db, "user.create").await;
    let row = created.last().unwrap();
    assert_eq!(
        (row.actor_kind(), row.actor_id(), row.actor_name()),
        ("user", Some(root_id), Some("root"))
    );
    assert_eq!(
        (row.target_kind(), row.target_name()),
        (Some("user"), Some("bob"))
    );
    assert_eq!(row.client_ip(), Some("127.0.0.1"));
    assert_eq!(detail(row), json!({"server_admin": false}));

    let (status, _) = call(
        &app.url("/users/bob"),
        Method::PATCH,
        Some(&root),
        Some(json!({"display_name": "Robert", "disabled": true, "password": "pw-654321"})),
    )
    .await;
    assert_eq!(status, 200);
    let updated = rows(&app.db, "user.update").await;
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].target_name(), Some("bob"));
    assert_eq!(
        detail(&updated[0]),
        json!({"fields": ["display_name", "disabled", "password"], "disabled": true})
    );

    let (status, _) = call(
        &app.url("/users/bob"),
        Method::PATCH,
        Some(&root),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = call(
        &app.url("/users/ghost"),
        Method::PATCH,
        Some(&root),
        Some(json!({"display_name": "G"})),
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(
        rows(&app.db, "user.update").await.len(),
        1,
        "no-ops write nothing"
    );
}

#[tokio::test]
async fn the_system_actor_has_no_id_and_no_address() {
    let app = test_app().await;
    app.db
        .create_user(
            potluck::db::accounts::NewUser::new("eve", "Eve"),
            &AuditCtx::system(),
        )
        .await
        .unwrap();
    let row = rows(&app.db, "user.create").await.pop().unwrap();
    assert_eq!(
        (row.actor_kind(), row.actor_id(), row.actor_name()),
        ("system", None, Some("system"))
    );
    assert_eq!(row.client_ip(), None);
}

#[tokio::test]
async fn a_failed_insert_rolls_the_user_change_back() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    boom(app._dir.path());
    let (status, _) = call(
        &app.url("/users"),
        Method::POST,
        Some(&root),
        Some(json!({"username": "carol", "display_name": "Carol", "password": "pw-123456"})),
    )
    .await;
    assert_eq!(status, 500);
    assert!(
        app.db.user_by_username("carol").await.unwrap().is_none(),
        "no user without its row"
    );
    let err = app
        .db
        .update_user(
            "root",
            potluck::db::accounts::UserPatch::default().display_name("R"),
            &AuditCtx::system(),
        )
        .await;
    assert!(err.is_err());
    let root_row = app.db.user_by_username("root").await.unwrap().unwrap();
    assert_eq!(root_row.display_name(), "root", "the patch rolled back");
}

#[tokio::test]
async fn signing_in_writes_session_create_and_no_token_create() {
    let app = test_app().await;
    let (bob, _) = api_user(&app.db, "bob").await;
    let before = rows(&app.db, "token.create").await.len();
    let (status, body) = call(
        &app.url("/session"),
        Method::POST,
        None,
        Some(json!({"username": "bob", "password": "pw-123456", "token_name": "desk"})),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let row = rows(&app.db, "session.create").await.pop().unwrap();
    assert_eq!((row.actor_kind(), row.actor_id()), ("user", Some(bob.id())));
    assert_eq!(
        (row.target_kind(), row.target_id(), row.target_name()),
        (
            Some("token"),
            body["token_meta"]["id"].as_i64(),
            Some("desk")
        )
    );
    assert_eq!(row.client_ip(), Some("127.0.0.1"));
    assert_eq!(row.detail(), None);
    assert_eq!(
        rows(&app.db, "token.create").await.len(),
        before,
        "one row per minted token"
    );
}

#[tokio::test]
async fn creating_and_revoking_a_token_names_its_owner() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let (_, bob) = api_user(&app.db, "bob").await;
    let (status, body) = call(
        &app.url("/tokens"),
        Method::POST,
        Some(&bob),
        Some(json!({"name": "ci"})),
    )
    .await;
    assert_eq!(status, 201);
    let id = body["token_meta"]["id"].as_i64().unwrap();
    let row = rows(&app.db, "token.create").await.pop().unwrap();
    assert_eq!(
        (row.actor_name(), row.target_id(), row.target_name()),
        (Some("bob"), Some(id), Some("ci"))
    );
    assert_eq!(detail(&row), json!({"user": "bob"}));

    let (status, _) = call(
        &app.url(&format!("/tokens/{id}")),
        Method::DELETE,
        Some(&root),
        None,
    )
    .await;
    assert_eq!(status, 204);
    let (status, _) = call(
        &app.url(&format!("/tokens/{id}")),
        Method::DELETE,
        Some(&root),
        None,
    )
    .await;
    assert_eq!(status, 404);
    let revoked = rows(&app.db, "token.revoke").await;
    assert_eq!(revoked.len(), 1, "the 404 writes nothing");
    assert_eq!(
        (revoked[0].actor_name(), revoked[0].target_name()),
        (Some("root"), Some("ci"))
    );
    assert_eq!(detail(&revoked[0]), json!({"user": "bob"}));
}

#[tokio::test]
async fn a_failed_insert_mints_no_token() {
    let app = test_app().await;
    let (bob_user, bob) = api_user(&app.db, "bob").await;
    let count = app.db.tokens_for_user(bob_user.id()).await.unwrap().len();
    boom(app._dir.path());
    let (status, _) = call(
        &app.url("/tokens"),
        Method::POST,
        Some(&bob),
        Some(json!({"name": "ci"})),
    )
    .await;
    assert_eq!(status, 500);
    assert_eq!(
        app.db.tokens_for_user(bob_user.id()).await.unwrap().len(),
        count
    );
}

async fn sign_in(app: &TestApp, username: &str, password: &str) -> u16 {
    call(
        &app.url("/session"),
        Method::POST,
        None,
        Some(json!({"username": username, "password": password, "token_name": "t"})),
    )
    .await
    .0
}

#[tokio::test]
async fn a_wrong_password_names_the_user_and_the_reason() {
    let app = test_app().await;
    let (bob, _) = api_user(&app.db, "bob").await;
    assert_eq!(sign_in(&app, "bob", "nope-nope").await, 401);
    let row = rows(&app.db, "session.failed").await.pop().unwrap();
    assert_eq!(
        (row.actor_kind(), row.actor_id(), row.actor_name()),
        ("anonymous", None, None)
    );
    assert_eq!(
        (row.target_kind(), row.target_id(), row.target_name()),
        (Some("user"), Some(bob.id()), Some("bob"))
    );
    assert_eq!(row.client_ip(), Some("127.0.0.1"));
    assert_eq!(detail(&row), json!({"reason": "wrong_password"}));
}

#[tokio::test]
async fn an_unknown_name_is_stored_nowhere() {
    let app = test_app().await;
    assert_eq!(sign_in(&app, "pasted-secret-xyz", "pw").await, 401);
    let row = rows(&app.db, "session.failed").await.pop().unwrap();
    assert_eq!((row.target_kind(), row.target_name()), (None, None));
    assert_eq!(detail(&row), json!({"reason": "unknown_user"}));
    assert!(!row.detail().unwrap().contains("pasted"));
}

#[tokio::test]
async fn disabled_and_passwordless_users_have_their_reasons() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_user(&app.db, "bob").await;
    let (status, _) = call(
        &app.url("/users/bob"),
        Method::PATCH,
        Some(&root),
        Some(json!({"disabled": true})),
    )
    .await;
    assert_eq!(status, 200);
    app.db
        .create_user(
            potluck::db::accounts::NewUser::new("np", "No Password"),
            &AuditCtx::system(),
        )
        .await
        .unwrap();
    assert_eq!(sign_in(&app, "bob", "pw-123456").await, 401);
    assert_eq!(sign_in(&app, "np", "anything").await, 401);
    let reasons: Vec<Value> = rows(&app.db, "session.failed")
        .await
        .iter()
        .map(|r| detail(r)["reason"].clone())
        .collect();
    assert_eq!(reasons, [json!("disabled"), json!("no_password")]);
}

#[tokio::test]
async fn rate_limited_attempts_write_nothing() {
    let app = test_app().await;
    api_user(&app.db, "ann").await;
    let mut statuses = Vec::new();
    for _ in 0..6 {
        statuses.push(sign_in(&app, "ann", "wrong-pw").await);
    }
    assert_eq!(statuses, [401, 401, 401, 401, 401, 429]);
    assert_eq!(rows(&app.db, "session.failed").await.len(), 5);
}

#[tokio::test]
async fn a_failing_log_still_answers_401_and_blocks_success() {
    let app = test_app().await;
    api_user(&app.db, "bob").await;
    boom(app._dir.path());
    assert_eq!(
        sign_in(&app, "bob", "wrong-pw").await,
        401,
        "the refusal does not change"
    );
    assert_eq!(
        sign_in(&app, "bob", "pw-123456").await,
        500,
        "no token without its row"
    );
}

#[tokio::test]
async fn team_changes_write_rows_only_when_something_changed() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_user(&app.db, "bob").await;
    api_team(&app.url(""), &root, "core").await;
    let created = rows(&app.db, "team.create").await;
    assert_eq!(created.len(), 1);
    assert_eq!(
        (
            created[0].actor_name(),
            created[0].target_kind(),
            created[0].target_name()
        ),
        (Some("root"), Some("team"), Some("core"))
    );
    for _ in 0..2 {
        let (status, _) = call(
            &app.url("/teams/core/members/bob"),
            Method::PUT,
            Some(&root),
            None,
        )
        .await;
        assert_eq!(status, 200);
    }
    let added = rows(&app.db, "team.member_add").await;
    assert_eq!(added.len(), 1, "an existing member is not added twice");
    assert_eq!(detail(&added[0]), json!({"user": "bob"}));
    let (status, _) = call(
        &app.url("/teams/core/members/bob"),
        Method::DELETE,
        Some(&root),
        None,
    )
    .await;
    assert_eq!(status, 204);
    let (status, _) = call(
        &app.url("/teams/core/members/bob"),
        Method::DELETE,
        Some(&root),
        None,
    )
    .await;
    assert_eq!(status, 404);
    let removed = rows(&app.db, "team.member_remove").await;
    assert_eq!(removed.len(), 1);
    assert_eq!(
        (removed[0].target_name(), detail(&removed[0])),
        (Some("core"), json!({"user": "bob"}))
    );
}

#[tokio::test]
async fn a_failed_insert_creates_no_team() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    boom(app._dir.path());
    let (status, _) = call(
        &app.url("/teams"),
        Method::POST,
        Some(&root),
        Some(json!({"slug": "core", "name": "Core"})),
    )
    .await;
    assert_eq!(status, 500);
    assert!(app.db.team_by_slug("core").await.unwrap().is_none());
}

#[tokio::test]
async fn creating_and_deleting_a_workspace_snapshot_its_name() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_team(&app.url(""), &root, "core").await;
    api_workspace(&app.url(""), &root, "core", "api", false).await;
    let created = rows(&app.db, "workspace.create").await;
    assert_eq!(created.len(), 1);
    assert_eq!(
        (
            created[0].target_kind(),
            created[0].target_name(),
            created[0].workspace()
        ),
        (Some("workspace"), Some("core/api"), Some("core/api"))
    );
    assert_eq!(
        detail(&created[0]),
        json!({"kind": "apinata", "seed": false})
    );
    let (status, _) = call(
        &app.url("/workspaces/core/api"),
        Method::DELETE,
        Some(&root),
        None,
    )
    .await;
    assert_eq!(status, 204);
    let deleted = rows(&app.db, "workspace.delete").await;
    assert_eq!(deleted.len(), 1);
    assert_eq!(
        (deleted[0].workspace(), deleted[0].target_id()),
        (Some("core/api"), created[0].target_id())
    );
}

#[tokio::test]
async fn a_failed_insert_leaves_no_workspace_and_no_directory() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_team(&app.url(""), &root, "core").await;
    boom(app._dir.path());
    let body = json!({"slug": "api", "name": "api", "kind": "apinata", "default_branch": "main"});
    let (status, _) = call(
        &app.url("/teams/core/workspaces"),
        Method::POST,
        Some(&root),
        Some(body),
    )
    .await;
    assert_eq!(status, 500);
    assert!(
        app.db
            .workspace_by_slugs("core", "api")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !crate::common::repo_path(app._dir.path(), "core", "api").exists(),
        "the claimed directory is removed"
    );
}

#[tokio::test]
async fn a_failed_insert_leaves_the_membership_unchanged() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_user(&app.db, "bob").await;
    api_user(&app.db, "carol").await;
    api_team(&app.url(""), &root, "core").await;
    let (status, _) = call(
        &app.url("/teams/core/members/bob"),
        Method::PUT,
        Some(&root),
        None,
    )
    .await;
    assert_eq!(status, 200);
    let team = app.db.team_by_slug("core").await.unwrap().unwrap().id();
    let before = app.db.team_members(team).await.unwrap();
    assert_eq!(before, ["bob"]);
    boom(app._dir.path());
    let (status, _) = call(
        &app.url("/teams/core/members/carol"),
        Method::PUT,
        Some(&root),
        None,
    )
    .await;
    assert_eq!(status, 500, "no member without its row");
    let (status, _) = call(
        &app.url("/teams/core/members/bob"),
        Method::DELETE,
        Some(&root),
        None,
    )
    .await;
    assert_eq!(status, 500, "no removal without its row");
    assert_eq!(app.db.team_members(team).await.unwrap(), before);
}

#[tokio::test]
async fn a_failed_insert_keeps_the_workspace_and_its_directory() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_team(&app.url(""), &root, "core").await;
    api_workspace(&app.url(""), &root, "core", "api", false).await;
    boom(app._dir.path());
    let (status, _) = call(
        &app.url("/workspaces/core/api"),
        Method::DELETE,
        Some(&root),
        None,
    )
    .await;
    assert_eq!(status, 500);
    assert!(
        app.db
            .workspace_by_slugs("core", "api")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        crate::common::repo_path(app._dir.path(), "core", "api").exists(),
        "the repository stays with its row"
    );
}

/// A grant row's target kind, target name, workspace and detail.
type GrantRow<'a> = (Option<&'a str>, Option<&'a str>, Option<&'a str>, Value);

async fn grant(app: &TestApp, token: &str, path: &str, method: Method, role: Option<&str>) -> u16 {
    call(
        &app.url(&format!("/workspaces/core/api/grants/{path}")),
        method,
        Some(token),
        role.map(|r| json!({"role": r})),
    )
    .await
    .0
}

#[tokio::test]
async fn grants_record_old_and_new_roles() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_user(&app.db, "bob").await;
    api_team(&app.url(""), &root, "core").await;
    api_workspace(&app.url(""), &root, "core", "api", true).await;
    assert_eq!(
        grant(&app, &root, "user/bob", Method::PUT, Some("write")).await,
        200
    );
    assert_eq!(
        grant(&app, &root, "user/bob", Method::PUT, Some("admin")).await,
        200
    );
    assert_eq!(
        grant(&app, &root, "team/core", Method::PUT, Some("read")).await,
        200
    );
    let put = rows(&app.db, "grant.put").await;
    let got: Vec<GrantRow<'_>> = put
        .iter()
        .map(|r| (r.target_kind(), r.target_name(), r.workspace(), detail(r)))
        .collect();
    assert_eq!(
        got,
        [
            (
                Some("user"),
                Some("bob"),
                Some("core/api"),
                json!({"old": null, "new": "write"})
            ),
            (
                Some("user"),
                Some("bob"),
                Some("core/api"),
                json!({"old": "write", "new": "admin"})
            ),
            (
                Some("team"),
                Some("core"),
                Some("core/api"),
                json!({"old": null, "new": "read"})
            ),
        ]
    );
    assert_eq!(
        grant(&app, &root, "user/bob", Method::DELETE, None).await,
        204
    );
    assert_eq!(
        grant(&app, &root, "user/bob", Method::DELETE, None).await,
        404
    );
    assert_eq!(
        grant(&app, &root, "user/ghost", Method::PUT, Some("read")).await,
        404
    );
    let deleted = rows(&app.db, "grant.delete").await;
    assert_eq!(deleted.len(), 1);
    assert_eq!(detail(&deleted[0]), json!({"old": "admin"}));
    assert_eq!(
        rows(&app.db, "grant.put").await.len(),
        3,
        "the unknown subject wrote nothing"
    );
}

#[tokio::test]
async fn a_failed_insert_leaves_the_grant_unchanged() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_user(&app.db, "bob").await;
    api_team(&app.url(""), &root, "core").await;
    api_workspace(&app.url(""), &root, "core", "api", true).await;
    assert_eq!(
        grant(&app, &root, "user/bob", Method::PUT, Some("read")).await,
        200
    );
    boom(app._dir.path());
    assert_eq!(
        grant(&app, &root, "user/bob", Method::PUT, Some("admin")).await,
        500
    );
    let (status, body) = call(
        &app.url("/workspaces/core/api/grants"),
        Method::GET,
        Some(&root),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!([{"subject": {"kind": "user", "name": "bob"}, "role": "read"}])
    );
}

#[tokio::test]
async fn arming_and_disarming_write_rows_and_consumed_or_missing_do_not() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_user(&app.db, "bob").await;
    api_team(&app.url(""), &root, "core").await;
    api_workspace(&app.url(""), &root, "core", "api", true).await;
    let (status, body) = call(
        &app.url("/workspaces/core/api/overrides"),
        Method::POST,
        Some(&root),
        Some(json!({"for_user": "bob", "reason": "vendor key fixture"})),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let id = body["id"].as_i64().unwrap();
    let armed = rows(&app.db, "override.arm").await;
    assert_eq!(armed.len(), 1);
    assert_eq!(
        (
            armed[0].target_kind(),
            armed[0].target_id(),
            armed[0].target_name(),
            armed[0].workspace()
        ),
        (Some("override"), Some(id), Some("bob"), Some("core/api"))
    );
    let url = app.url(&format!("/workspaces/core/api/overrides/{id}"));
    assert_eq!(call(&url, Method::DELETE, Some(&root), None).await.0, 204);
    assert_eq!(call(&url, Method::DELETE, Some(&root), None).await.0, 404);
    let disarmed = rows(&app.db, "override.disarm").await;
    assert_eq!(disarmed.len(), 1);
    assert_eq!(
        (disarmed[0].actor_name(), disarmed[0].target_id()),
        (Some("root"), Some(id))
    );
}

#[tokio::test]
async fn a_consuming_push_writes_override_use_once() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let (bob, _) = api_user(&app.db, "bob").await;
    api_team(&app.url(""), &root, "core").await;
    let ws = api_workspace(&app.url(""), &root, "core", "api", true).await;
    assert_eq!(ws["slug"], "api");
    let ws_id = app
        .db
        .workspace_by_slugs("core", "api")
        .await
        .unwrap()
        .unwrap()
        .id();
    let id = app
        .db
        .arm_override(ws_id, bob.id(), "fixture", "system", &AuditCtx::system())
        .await
        .unwrap();
    let a = "a".repeat(40);
    let moves = [
        potluck::git::push::RefMove::new("refs/heads/main", potluck::git::push::ZERO_SHA, &a),
        potluck::git::push::RefMove::new("refs/heads/topic", potluck::git::push::ZERO_SHA, &a),
    ];
    assert!(
        app.db
            .record_push(ws_id, Some(id), &moves, "bob")
            .await
            .unwrap()
            .override_consumed()
    );
    assert!(
        !app.db
            .record_push(ws_id, Some(id), &moves, "bob")
            .await
            .unwrap()
            .override_consumed()
    );
    app.db
        .record_push(ws_id, None, &moves, "bob")
        .await
        .unwrap();
    let used = rows(&app.db, "override.use").await;
    assert_eq!(used.len(), 1, "only the consuming push");
    assert_eq!(
        (
            used[0].actor_kind(),
            used[0].actor_id(),
            used[0].actor_name()
        ),
        ("user", Some(bob.id()), Some("bob"))
    );
    assert_eq!(
        (
            used[0].target_id(),
            used[0].workspace(),
            used[0].client_ip()
        ),
        (Some(id), Some("core/api"), None)
    );
    assert_eq!(detail(&used[0]), json!({"refs": 2}));
    let url = app.url(&format!("/workspaces/core/api/overrides/{id}"));
    assert_eq!(
        call(&url, Method::DELETE, Some(&root), None).await.0,
        409,
        "a consumed override stays in the history"
    );
    assert!(
        rows(&app.db, "override.disarm").await.is_empty(),
        "a refused disarm writes no row"
    );
}

#[tokio::test]
async fn a_failed_insert_keeps_the_override_armed() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let (bob, _) = api_user(&app.db, "bob").await;
    api_team(&app.url(""), &root, "core").await;
    api_workspace(&app.url(""), &root, "core", "api", true).await;
    let ws_id = app
        .db
        .workspace_by_slugs("core", "api")
        .await
        .unwrap()
        .unwrap()
        .id();
    let id = app
        .db
        .arm_override(ws_id, bob.id(), "fixture", "system", &AuditCtx::system())
        .await
        .unwrap();
    boom(app._dir.path());
    let a = "a".repeat(40);
    let moves = [potluck::git::push::RefMove::new(
        "refs/heads/main",
        potluck::git::push::ZERO_SHA,
        &a,
    )];
    assert!(
        app.db
            .record_push(ws_id, Some(id), &moves, "bob")
            .await
            .is_err()
    );
    assert_eq!(
        app.db.armed_override(ws_id, bob.id()).await.unwrap(),
        Some(id),
        "the consume rolled back"
    );
    assert!(
        app.db
            .disarm_override(ws_id, id, &AuditCtx::system())
            .await
            .is_err(),
        "disarm rolls back too"
    );
    assert_eq!(
        app.db.armed_override(ws_id, bob.id()).await.unwrap(),
        Some(id)
    );
}

#[tokio::test]
async fn no_audit_column_carries_a_secret() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let password = "canary-pw-7f3a91";
    let new_password = "canary-pw-2c84e0";
    let (status, _) = call(
        &app.url("/users"),
        Method::POST,
        Some(&root),
        Some(json!({"username": "bob", "display_name": "Bob", "password": password})),
    )
    .await;
    assert_eq!(status, 201);
    let (status, signed_in) = call(
        &app.url("/session"),
        Method::POST,
        None,
        Some(json!({"username": "bob", "password": password, "token_name": "desk"})),
    )
    .await;
    assert_eq!(status, 201);
    let session_secret = signed_in["token"].as_str().unwrap().to_owned();
    let (status, minted) = call(
        &app.url("/tokens"),
        Method::POST,
        Some(&session_secret),
        Some(json!({"name": "ci"})),
    )
    .await;
    assert_eq!(status, 201);
    let api_secret = minted["token"].as_str().unwrap().to_owned();
    let id = minted["token_meta"]["id"].as_i64().unwrap();
    assert_eq!(
        call(
            &app.url(&format!("/tokens/{id}")),
            Method::DELETE,
            Some(&session_secret),
            None
        )
        .await
        .0,
        204
    );
    // Salts are random, so the hash the user.create row saw differs from the one after the
    // password change: both are searched for.
    let bob = app.db.user_by_username("bob").await.unwrap().unwrap();
    let first_phc = bob.password_hash().unwrap().to_owned();
    let (status, _) = call(
        &app.url("/users/bob"),
        Method::PATCH,
        Some(&session_secret),
        Some(json!({"password": new_password, "current_password": password})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        call(
            &app.url("/session"),
            Method::POST,
            None,
            Some(json!({"username": "bob", "password": password, "token_name": "x"}))
        )
        .await
        .0,
        401
    );
    let phc = app
        .db
        .user_by_username("bob")
        .await
        .unwrap()
        .unwrap()
        .password_hash()
        .unwrap()
        .to_owned();

    assert_ne!(first_phc, phc, "the password change rehashed");

    let mut needles = vec![password.to_owned(), new_password.to_owned(), first_phc, phc];
    for secret in [&session_secret, &api_secret, &root] {
        needles.push(secret.clone());
        needles.push(secret[..8].to_owned());
        let digest = potluck::auth::token::digest(secret);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        needles.push(hex.to_uppercase());
        needles.push(hex);
    }
    let c = rusqlite::Connection::open(app._dir.path().join("server.db")).unwrap();
    let mut stmt = c.prepare("SELECT * FROM audit").unwrap();
    let width = stmt.column_count();
    let cells: Vec<String> = stmt
        .query_map([], |r| {
            // Render each cell as text: a ValueRef's Debug would print text as a byte list and
            // hide every needle.
            (0..width)
                .map(|i| {
                    Ok(match r.get_ref(i)? {
                        rusqlite::types::ValueRef::Text(t) => {
                            String::from_utf8_lossy(t).into_owned()
                        }
                        rusqlite::types::ValueRef::Integer(n) => n.to_string(),
                        rusqlite::types::ValueRef::Null => String::new(),
                        other => panic!("audit holds only text, integers and nulls: {other:?}"),
                    })
                })
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap()
        .flat_map(Result::unwrap)
        .collect();
    assert!(
        cells.len() > width * 5,
        "the scenario wrote rows: {}",
        cells.len() / width
    );
    assert!(cells.iter().any(|c| c == "bob"), "the scan sees text cells");
    for needle in &needles {
        assert!(
            !cells.iter().any(|c| c.contains(needle.as_str())),
            "a secret reached the audit log: {needle}"
        );
    }
}
