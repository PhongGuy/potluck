//! `PUT` and `DELETE /api/v1/users/{username}/oidc`.

use potluck::oidc::Timings;
use reqwest::Method;
use serde_json::json;

use crate::common::oidc::OidcApp;
use crate::common::{api_admin, api_user, call, create_user, test_app};

async fn app_with_admin() -> (OidcApp, String) {
    let app = OidcApp::start(false, Timings::default()).await;
    let admin = api_admin(&app.db).await;
    (app, admin)
}

#[tokio::test]
async fn put_links_and_replaces_and_delete_unlinks() {
    let (app, admin) = app_with_admin().await;
    create_user(&app.db, "ada", "pw-123456", false).await;
    let url = app.api("/users/ada/oidc");
    let (status, body) = call(
        &url,
        Method::PUT,
        Some(&admin),
        Some(json!({"subject": "sub-1"})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["issuer"], app.idp.issuer());
    assert_eq!(body["subject"], "sub-1");
    assert!(body["linked_at"].is_string());
    let (status, body) = call(
        &url,
        Method::PUT,
        Some(&admin),
        Some(json!({"subject": "sub-2"})),
    )
    .await;
    assert_eq!((status, body["subject"].as_str()), (200, Some("sub-2")));
    let ada = app.db.user_by_username("ada").await.unwrap().unwrap();
    assert_eq!(
        app.db
            .oidc_link_of(&app.idp.issuer(), ada.id())
            .await
            .unwrap()
            .unwrap()
            .subject(),
        "sub-2"
    );
    assert_eq!(call(&url, Method::DELETE, Some(&admin), None).await.0, 204);
    assert!(
        app.db
            .oidc_link_of(&app.idp.issuer(), ada.id())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        call(&url, Method::DELETE, Some(&admin), None).await.0,
        204,
        "idempotent"
    );
    assert_eq!(
        call(
            &app.api("/users/nobody/oidc"),
            Method::DELETE,
            Some(&admin),
            None
        )
        .await
        .0,
        404
    );
}

#[tokio::test]
async fn link_and_unlink_are_audited_once_each() {
    let (app, admin) = app_with_admin().await;
    create_user(&app.db, "ada", "pw-123456", false).await;
    let url = app.api("/users/ada/oidc");
    call(
        &url,
        Method::PUT,
        Some(&admin),
        Some(json!({"subject": "sub-1"})),
    )
    .await;
    call(&url, Method::DELETE, Some(&admin), None).await;
    call(&url, Method::DELETE, Some(&admin), None).await;
    let (_, links) = call(
        &app.api("/audit?action=user.oidc_link"),
        Method::GET,
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(links.as_array().unwrap().len(), 1);
    assert_eq!(links[0]["target_name"], "ada");
    assert_eq!(links[0]["actor_kind"], "user");
    assert_eq!(links[0]["actor_name"], "root");
    assert_eq!(links[0]["client_ip"], "127.0.0.1");
    assert_eq!(links[0]["detail"]["subject"], "sub-1");
    let (_, unlinks) = call(
        &app.api("/audit?action=user.oidc_unlink"),
        Method::GET,
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(
        unlinks.as_array().unwrap().len(),
        1,
        "the no-op delete wrote nothing"
    );
    assert_eq!(unlinks[0]["target_name"], "ada");
    assert_eq!(unlinks[0]["actor_name"], "root");
    assert_eq!(unlinks[0]["client_ip"], "127.0.0.1");
    assert_eq!(unlinks[0]["detail"]["subject"], "sub-1");
}

#[tokio::test]
async fn bad_requests_get_their_codes() {
    let (app, admin) = app_with_admin().await;
    create_user(&app.db, "ada", "pw-123456", false).await;
    create_user(&app.db, "bob", "pw-123456", false).await;
    let put = |user: &str| app.api(&format!("/users/{user}/oidc"));
    let (status, body) = call(
        &put("nobody"),
        Method::PUT,
        Some(&admin),
        Some(json!({"subject": "s"})),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (404, Some("not_found"))
    );
    for bad in [String::new(), "x".repeat(256)] {
        let (status, body) = call(
            &put("ada"),
            Method::PUT,
            Some(&admin),
            Some(json!({"subject": bad})),
        )
        .await;
        assert_eq!(
            (status, body["error"]["field"].as_str()),
            (422, Some("subject"))
        );
    }
    assert_eq!(
        call(
            &put("ada"),
            Method::PUT,
            Some(&admin),
            Some(json!({"subject": "x".repeat(255)}))
        )
        .await
        .0,
        200
    );
    call(
        &put("ada"),
        Method::PUT,
        Some(&admin),
        Some(json!({"subject": "shared"})),
    )
    .await;
    let (status, body) = call(
        &put("bob"),
        Method::PUT,
        Some(&admin),
        Some(json!({"subject": "shared"})),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (409, Some("conflict"))
    );
    // Refused writes leave no trace: only ada's two successful links are in the log.
    let (_, links) = call(
        &app.api("/audit?action=user.oidc_link"),
        Method::GET,
        Some(&admin),
        None,
    )
    .await;
    let links = links.as_array().unwrap();
    assert_eq!(links.len(), 2, "{links:?}");
    assert!(links.iter().all(|row| row["target_name"] == "ada"));
}

#[tokio::test]
async fn only_server_admins_link() {
    let app = OidcApp::start(false, Timings::default()).await;
    let (_ada, token) = api_user(&app.db, "ada").await;
    let url = app.api("/users/ada/oidc");
    assert_eq!(
        call(
            &url,
            Method::PUT,
            Some(&token),
            Some(json!({"subject": "s"}))
        )
        .await
        .0,
        403
    );
    assert_eq!(call(&url, Method::DELETE, Some(&token), None).await.0, 403);
    assert_eq!(
        call(&url, Method::PUT, None, Some(json!({"subject": "s"})))
            .await
            .0,
        401
    );
}

#[tokio::test]
async fn without_oidc_linking_is_a_conflict() {
    let app = test_app().await;
    let admin = api_admin(&app.db).await;
    create_user(&app.db, "ada", "pw-123456", false).await;
    let (status, body) = call(
        &app.url("/users/ada/oidc"),
        Method::PUT,
        Some(&admin),
        Some(json!({"subject": "s"})),
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (409, Some("conflict"))
    );
    assert!(body["error"]["message"].as_str().unwrap().contains("OIDC"));
}
