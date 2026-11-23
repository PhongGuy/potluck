//! The readable workspace list, `GET W`, `/me` carrying the same list, and `System`.

use potluck::db::accounts::UserPatch;
use potluck::db::audit::AuditCtx;
use potluck::db::grants::Subject;
use potluck::db::workspaces::NewWorkspace;
use potluck::role::Role;
use potluck::slug::Slug;
use reqwest::Method;
use serde_json::{Value, json};

use crate::common::{
    api_admin, api_team, api_user, api_workspace, call, client, config_for, test_app, unix_get,
};

#[tokio::test]
async fn the_list_and_me_show_held_workspaces_with_the_highest_role() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let api = app.url("");
    api_team(&api, &root, "core").await;
    api_workspace(&api, &root, "core", "api", true).await;
    api_workspace(&api, &root, "core", "docs", true).await;
    let (bob_rec, bob) = api_user(&app.db, "bob").await;
    let (_, ann) = api_user(&app.db, "ann").await;
    let core = app.db.team_by_slug("core").await.unwrap().unwrap();
    let api_ws = app
        .db
        .workspace_by_slugs("core", "api")
        .await
        .unwrap()
        .unwrap();
    let docs = app
        .db
        .workspace_by_slugs("core", "docs")
        .await
        .unwrap()
        .unwrap();
    app.db
        .put_grant(docs.id(), Subject::User("bob".into()), Role::Read)
        .await
        .unwrap();
    app.db
        .add_member(core.id(), bob_rec.id(), &AuditCtx::system())
        .await
        .unwrap();
    app.db
        .put_grant(api_ws.id(), Subject::Team("core".into()), Role::Write)
        .await
        .unwrap();
    app.db
        .put_grant(api_ws.id(), Subject::User("bob".into()), Role::Read)
        .await
        .unwrap();
    let (status, list) = call(&app.url("/workspaces"), Method::GET, Some(&bob), None).await;
    assert_eq!(status, 200, "{list}");
    let seen: Vec<(&str, &str)> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|w| (w["slug"].as_str().unwrap(), w["role"].as_str().unwrap()))
        .collect();
    assert_eq!(seen, [("api", "write"), ("docs", "read")]);
    let (_, me) = call(&app.url("/me"), Method::GET, Some(&bob), None).await;
    assert_eq!(me["workspaces"], list, "/me carries the same list");
    let (_, filtered) = call(
        &app.url("/workspaces?kind=apinata"),
        Method::GET,
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(filtered, list);
    let (_, none) = call(
        &app.url("/workspaces?kind=dbento"),
        Method::GET,
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(none, json!([]));
    let (_, empty) = call(&app.url("/workspaces"), Method::GET, Some(&ann), None).await;
    assert_eq!(empty, json!([]), "no grants, no workspaces");
    let (_, all) = call(&app.url("/workspaces"), Method::GET, Some(&root), None).await;
    assert_eq!(all.as_array().unwrap().len(), 2);
    assert!(
        all.as_array().unwrap().iter().all(|w| w["role"] == "admin"),
        "server admins are admin everywhere: {all}"
    );
    let (status, _) = call(&app.url("/workspaces"), Method::GET, None, None).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn get_workspace_needs_read_and_hides_what_the_caller_cannot_see() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_team(&app.url(""), &root, "core").await;
    api_workspace(&app.url(""), &root, "core", "api", true).await;
    let (_, bob) = api_user(&app.db, "bob").await;
    let url = app.url("/workspaces/core/api");
    let (status, err) = call(&url, Method::GET, Some(&bob), None).await;
    assert_eq!(status, 404, "no role is not found: {err}");
    assert_eq!(err["error"]["code"], "not_found");
    let ws = app
        .db
        .workspace_by_slugs("core", "api")
        .await
        .unwrap()
        .unwrap();
    app.db
        .put_grant(ws.id(), Subject::User("bob".into()), Role::Read)
        .await
        .unwrap();
    let (status, body) = call(&url, Method::GET, Some(&bob), None).await;
    assert_eq!(
        (status, &body["role"], &body["slug"]),
        (200, &json!("read"), &json!("api")),
        "{body}"
    );
    let (_, body) = call(&url, Method::GET, Some(&root), None).await;
    assert_eq!(body["role"], "admin");
    for path in [
        "/workspaces/core/nope",
        "/workspaces/Core/api",
        "/workspaces/core/api.git",
    ] {
        let (status, _) = call(&app.url(path), Method::GET, Some(&root), None).await;
        assert_eq!(status, 404, "{path}");
    }
    let (status, _) = call(&url, Method::GET, None, None).await;
    assert_eq!(status, 401);
    app.db
        .update_user(
            "bob",
            UserPatch::default().disabled(true),
            &AuditCtx::system(),
        )
        .await
        .unwrap();
    let (status, err) = call(&url, Method::GET, Some(&bob), None).await;
    assert_eq!(
        status, 401,
        "a disabled user's grant no longer opens anything: {err}"
    );
    assert_eq!(err["error"]["code"], "unauthenticated");
}

#[tokio::test]
async fn the_admin_socket_sees_every_workspace_as_admin() {
    let dir = tempfile::tempdir().unwrap();
    let running = potluck::server::start(&config_for(dir.path()))
        .await
        .unwrap();
    let core = running
        .db()
        .create_team(&Slug::parse("core").unwrap(), "Core", &AuditCtx::system())
        .await
        .unwrap();
    running
        .db()
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
    let (status, body) = unix_get(running.admin_socket(), "/api/v1/workspaces").await;
    assert_eq!(status, 200, "{body}");
    let list: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (list[0]["slug"].as_str(), list[0]["role"].as_str()),
        (Some("api"), Some("admin")),
        "{body}"
    );
    let (status, body) = unix_get(running.admin_socket(), "/api/v1/workspaces/core/api").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["role"],
        "admin"
    );
    let (status, _) = unix_get(running.admin_socket(), "/api/v1/me").await;
    assert_eq!(status, 403, "/me keeps refusing System");
    running.shutdown().await;
}

/// A workspace the caller holds no role on answers byte for byte like one that does not exist,
/// so the 404 cannot be used to probe which workspaces exist.
#[tokio::test]
async fn get_workspace_answers_no_role_exactly_like_no_workspace() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_team(&app.url(""), &root, "core").await;
    api_workspace(&app.url(""), &root, "core", "api", false).await;
    let (_, bob) = api_user(&app.db, "bob").await;
    let mut answers = Vec::new();
    for path in [
        "/workspaces/core/api",
        "/workspaces/core/nope",
        "/workspaces/other/api",
    ] {
        let res = client()
            .get(app.url(path))
            .bearer_auth(&bob)
            .send()
            .await
            .unwrap();
        let status = res.status().as_u16();
        let content_type = res.headers().get("content-type").cloned();
        answers.push((status, content_type, res.bytes().await.unwrap()));
    }
    assert_eq!(answers[0].0, 404, "{:?}", answers[0]);
    assert_eq!(answers[0], answers[1], "no role and an unknown workspace");
    assert_eq!(answers[0], answers[2], "no role and an unknown team");
}
