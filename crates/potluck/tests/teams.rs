//! Teams and members, managed by server admins.

use reqwest::Method;
use serde_json::json;

use crate::common::{api_admin, api_team, api_user, call, config_for, test_app, unix_get};

#[tokio::test]
async fn server_admins_create_and_list_teams() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let teams = app.url("/teams");
    assert_eq!(
        call(&teams, Method::GET, Some(&root), None).await,
        (200, json!([]))
    );
    let (status, body) = call(
        &teams,
        Method::POST,
        Some(&root),
        Some(json!({"slug": "core", "name": " Core "})),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        body,
        json!({"slug": "core", "name": "Core", "members": []}),
        "the name is trimmed"
    );
    api_team(&app.url(""), &root, "api").await;
    let (_, listed) = call(&teams, Method::GET, Some(&root), None).await;
    assert_eq!(listed[0]["slug"], "api");
    assert_eq!(listed[1]["slug"], "core");
    let (status, err) = call(
        &teams,
        Method::POST,
        Some(&root),
        Some(json!({"slug": "core", "name": "Again"})),
    )
    .await;
    assert_eq!(status, 409, "{err}");
    assert_eq!(err["error"]["code"], "conflict");
    for (body, field) in [
        (json!({"slug": "Core", "name": "x"}), "slug"),
        (json!({"slug": "-x", "name": "x"}), "slug"),
        (json!({"slug": "system-ok", "name": "   "}), "name"),
        (json!({"slug": "ok", "name": "n".repeat(201)}), "name"),
    ] {
        let (status, err) = call(&teams, Method::POST, Some(&root), Some(body.clone())).await;
        assert_eq!(status, 422, "{body}: {err}");
        assert_eq!(err["error"]["field"], field, "{err}");
    }
    let (status, err) = call(
        &teams,
        Method::POST,
        Some(&root),
        Some(json!({"slug": "ok"})),
    )
    .await;
    assert_eq!(status, 400, "{err}");
    assert!(
        err["error"]["message"].as_str().unwrap().contains("name"),
        "{err}"
    );
    assert_eq!(app.db.list_teams().await.unwrap().len(), 2);
}

#[tokio::test]
async fn teams_need_a_server_admin() {
    let app = test_app().await;
    let (_, bob) = api_user(&app.db, "bob").await;
    for (method, path, body) in [
        (Method::GET, "/teams", None),
        (
            Method::POST,
            "/teams",
            Some(json!({"slug": "core", "name": "Core"})),
        ),
        (Method::POST, "/teams", Some(json!({}))),
        (Method::PUT, "/teams/core/members/bob", None),
        (Method::DELETE, "/teams/core/members/bob", None),
    ] {
        let (status, err) = call(&app.url(path), method.clone(), Some(&bob), body.clone()).await;
        assert_eq!(
            status, 403,
            "{method} {path} {body:?}: the role is checked before the body: {err}"
        );
        assert_eq!(err["error"]["code"], "forbidden");
        let (status, _) = call(&app.url(path), method.clone(), None, body).await;
        assert_eq!(status, 401, "{method} {path} anonymous");
    }
    assert!(
        app.db.list_teams().await.unwrap().is_empty(),
        "nothing was created"
    );
}

#[tokio::test]
async fn members_are_added_and_removed() {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    api_user(&app.db, "bob").await;
    api_user(&app.db, "ann").await;
    api_team(&app.url(""), &root, "core").await;
    let member = |who: &str| app.url(&format!("/teams/core/members/{who}"));
    let (status, team) = call(&member("bob"), Method::PUT, Some(&root), None).await;
    assert_eq!((status, &team["members"]), (200, &json!(["bob"])), "{team}");
    let (_, team) = call(&member("ann"), Method::PUT, Some(&root), None).await;
    assert_eq!(team["members"], json!(["ann", "bob"]));
    let (status, team) = call(&member("bob"), Method::PUT, Some(&root), None).await;
    assert_eq!(
        (status, &team["members"]),
        (200, &json!(["ann", "bob"])),
        "idempotent"
    );
    let (status, _) = call(&member("nobody"), Method::PUT, Some(&root), None).await;
    assert_eq!(status, 404);
    let (status, _) = call(
        &app.url("/teams/nope/members/bob"),
        Method::PUT,
        Some(&root),
        None,
    )
    .await;
    assert_eq!(status, 404);
    let (status, body) = call(&member("bob"), Method::DELETE, Some(&root), None).await;
    assert_eq!((status, body), (204, serde_json::Value::Null));
    let (status, _) = call(&member("bob"), Method::DELETE, Some(&root), None).await;
    assert_eq!(status, 404, "not a member any more");
    let (_, listed) = call(&app.url("/teams"), Method::GET, Some(&root), None).await;
    assert_eq!(listed[0]["members"], json!(["ann"]));
}

#[tokio::test]
async fn the_admin_socket_acts_as_system() {
    let dir = tempfile::tempdir().unwrap();
    let running = potluck::server::start(&config_for(dir.path()))
        .await
        .unwrap();
    let (status, body) = unix_get(running.admin_socket(), "/api/v1/teams").await;
    assert_eq!(
        (status, body.as_str()),
        (200, "[]"),
        "System lists teams without a token"
    );
    running.shutdown().await;
}
