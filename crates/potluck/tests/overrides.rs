//! Arming, listing and disarming overrides through the API.

use std::time::Duration;

use potluck::git::push::{RefMove, ZERO_SHA};
use reqwest::Method;
use serde_json::{Value, json};

use crate::common::{
    TestApp, api_admin, api_team, api_user, api_workspace, call, full_server, git, ok, test_app,
    work_tree, write_hook,
};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Arena {
    app: TestApp,
    root: String,
    bob: String,
    ann: String,
}

/// `core/api` made by `root`; `bob` holds write and `ann` read on it.
async fn arena() -> Arena {
    let app = test_app().await;
    let root = api_admin(&app.db).await;
    let (_, bob) = api_user(&app.db, "bob").await;
    let (_, ann) = api_user(&app.db, "ann").await;
    api_team(&app.url(""), &root, "core").await;
    api_workspace(&app.url(""), &root, "core", "api", true).await;
    for (name, role) in [("bob", "write"), ("ann", "read")] {
        let (status, _) = call(
            &app.url(&format!("/workspaces/core/api/grants/user/{name}")),
            Method::PUT,
            Some(&root),
            Some(json!({"role": role})),
        )
        .await;
        assert_eq!(status, 200);
    }
    Arena {
        app,
        root,
        bob,
        ann,
    }
}

async fn arm(a: &Arena, token: &str, body: Value) -> (u16, Value) {
    call(
        &a.app.url("/workspaces/core/api/overrides"),
        Method::POST,
        Some(token),
        Some(body),
    )
    .await
}

#[tokio::test]
async fn admins_arm_list_and_disarm() {
    let a = arena().await;
    let (status, ov) = arm(
        &a,
        &a.root,
        json!({"for_user": "bob", "reason": "  importing the old collection "}),
    )
    .await;
    assert_eq!(status, 201, "{ov}");
    assert_eq!(ov["workspace"], "core/api");
    assert_eq!(ov["for_user"], "bob");
    assert_eq!(
        ov["reason"], "importing the old collection",
        "stored trimmed"
    );
    assert_eq!(ov["armed_by"], "root");
    assert!(ov["armed_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(ov["consumed_at"], Value::Null);
    assert_eq!(ov["consumed_refs"], Value::Null);
    let id = ov["id"].as_i64().unwrap();
    let (status, again) = arm(&a, &a.root, json!({"for_user": "bob", "reason": "twice"})).await;
    assert_eq!(
        (status, again["error"]["code"].as_str()),
        (409, Some("conflict")),
        "{again}"
    );
    let (status, body) = arm(&a, &a.root, json!({"for_user": "nobody", "reason": "x"})).await;
    assert_eq!(
        (status, body["error"]["field"].as_str()),
        (422, Some("for_user")),
        "{body}"
    );
    let (status, body) = arm(&a, &a.root, json!({"for_user": "ann", "reason": "   "})).await;
    assert_eq!(
        (status, body["error"]["field"].as_str()),
        (422, Some("reason")),
        "{body}"
    );
    let (status, body) = arm(
        &a,
        &a.root,
        json!({"for_user": "ann", "reason": "r".repeat(1001)}),
    )
    .await;
    assert_eq!(
        (status, body["error"]["field"].as_str()),
        (422, Some("reason")),
        "{body}"
    );
    let (status, _) = arm(&a, &a.bob, json!({"for_user": "ann", "reason": "x"})).await;
    assert_eq!(status, 403, "write is not admin");
    let (status, _) = arm(&a, &a.ann, json!({"for_user": "bob", "reason": "x"})).await;
    assert_eq!(status, 403, "read is not admin");
    let (status, _) = call(
        &a.app.url("/users/ann"),
        Method::PATCH,
        Some(&a.root),
        Some(json!({"disabled": true})),
    )
    .await;
    assert_eq!(status, 200);
    let (status, body) = arm(&a, &a.root, json!({"for_user": "ann", "reason": "x"})).await;
    assert_eq!(
        (status, body["error"]["field"].as_str()),
        (422, Some("for_user")),
        "a disabled user cannot push, so nothing to override: {body}"
    );
    let (status, _) = arm(&a, "plk_unknown", json!({"for_user": "bob", "reason": "x"})).await;
    assert_eq!(status, 401);
    let (status, list) = call(
        &a.app.url("/workspaces/core/api/overrides"),
        Method::GET,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], id);
    let (status, _) = call(
        &a.app.url("/workspaces/core/api/overrides"),
        Method::GET,
        Some(&a.bob),
        None,
    )
    .await;
    assert_eq!(status, 403);
    let (status, _) = call(
        &a.app.url("/workspaces/core/nope/overrides"),
        Method::GET,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(status, 404);
    let (status, _) = call(
        &a.app.url(&format!("/workspaces/core/api/overrides/{id}")),
        Method::DELETE,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(status, 204);
    let (status, _) = call(
        &a.app.url(&format!("/workspaces/core/api/overrides/{id}")),
        Method::DELETE,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(status, 404, "gone");
    let (status, _) = call(
        &a.app.url("/workspaces/core/api/overrides/abc"),
        Method::DELETE,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(status, 404, "not a number");
    let (_, list) = call(
        &a.app.url("/workspaces/core/api/overrides"),
        Method::GET,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(list, json!([]));
}

#[tokio::test]
async fn a_consumed_override_shows_its_refs_and_cannot_be_disarmed() {
    let a = arena().await;
    let (status, ov) = arm(&a, &a.root, json!({"for_user": "bob", "reason": "import"})).await;
    assert_eq!(status, 201);
    let id = ov["id"].as_i64().unwrap();
    let ws = a
        .app
        .db
        .workspace_by_slugs("core", "api")
        .await
        .unwrap()
        .unwrap();
    let done = a
        .app
        .db
        .record_push(
            ws.id(),
            Some(id),
            &[RefMove::new("refs/heads/main", ZERO_SHA, A)],
        )
        .await
        .unwrap();
    assert!(done.override_consumed());
    let (status, body) = call(
        &a.app.url(&format!("/workspaces/core/api/overrides/{id}")),
        Method::DELETE,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (409, Some("conflict")),
        "{body}"
    );
    let (_, list) = call(
        &a.app.url("/workspaces/core/api/overrides"),
        Method::GET,
        Some(&a.root),
        None,
    )
    .await;
    assert_eq!(
        list[0]["consumed_refs"],
        json!([{"ref": "refs/heads/main", "old": ZERO_SHA, "new": A}])
    );
    assert!(list[0]["consumed_at"].is_string());
    let (status, _) = arm(
        &a,
        &a.root,
        json!({"for_user": "bob", "reason": "once more"}),
    )
    .await;
    assert_eq!(status, 201, "a consumed override does not block a new one");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_override_armed_through_the_api_is_consumed_by_the_users_next_push() {
    let server = full_server().await;
    let root = api_admin(server.db()).await;
    let (_, bob) = api_user(server.db(), "bob").await;
    api_team(&server.api(""), &root, "core").await;
    api_workspace(&server.api(""), &root, "core", "api", false).await;
    let (status, _) = call(
        &server.api("/workspaces/core/api/grants/user/bob"),
        Method::PUT,
        Some(&root),
        Some(json!({"role": "write"})),
    )
    .await;
    assert_eq!(status, 200);
    let (status, ov) = call(
        &server.api("/workspaces/core/api/overrides"),
        Method::POST,
        Some(&root),
        Some(json!({"for_user": "bob", "reason": "import"})),
    )
    .await;
    assert_eq!(status, 201, "{ov}");
    let id = ov["id"].as_i64().unwrap();
    // The stub hook accepts only when the production resolver passed this override's id.
    write_hook(
        &server.hooks_dir(),
        &format!("[ \"$POTLUCK_OVERRIDE\" = \"{id}\" ] || exit 1"),
    );
    let home = server.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let work = work_tree(&home, "work");
    let tip = ok(git(&home, Some(&work), None, &["rev-parse", "HEAD"]))
        .trim()
        .to_owned();
    ok(git(
        &home,
        Some(&work),
        Some(&bob),
        &["push", "-q", &server.git_url("core", "api"), "main"],
    ));
    // The record lands after git exits, so poll the list until it shows the consume.
    let consumed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (status, list) = call(
                &server.api("/workspaces/core/api/overrides"),
                Method::GET,
                Some(&root),
                None,
            )
            .await;
            assert_eq!(status, 200, "{list}");
            if list[0]["consumed_at"].is_string() {
                return list;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the push consumes the override within 10 s");
    assert_eq!(consumed[0]["id"], id);
    assert_eq!(
        consumed[0]["consumed_refs"],
        json!([{"ref": "refs/heads/main", "old": ZERO_SHA, "new": tip}])
    );
}
