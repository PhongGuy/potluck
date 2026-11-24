//! `GET /api/v1/oidc/callback`: where the browser is sent for each outcome.

use std::time::Duration;

use potluck::db::accounts::UserPatch;
use potluck::db::audit::AuditCtx;
use potluck::oidc::Timings;

use crate::common::create_user;
use crate::common::idp::{Fault, Identity};
use crate::common::oidc::{APP_STATE, OidcApp, VERIFIER, assert_page, challenge, param};

async fn linked_app(subject: &str) -> OidcApp {
    let app = OidcApp::start(false, Timings::default()).await;
    let ada = create_user(&app.db, "ada", "pw-123456", false).await;
    app.db
        .put_oidc_link(&app.idp.issuer(), ada.id(), subject, &AuditCtx::system())
        .await
        .unwrap();
    app.idp.set_identity(Identity::new(subject));
    app
}

#[tokio::test]
async fn a_linked_user_gets_a_code_on_the_v4_loopback() {
    let app = linked_app("sub-ada").await;
    let to = app.sign_in_to_loopback().await;
    assert_eq!(
        format!(
            "{}://{}:{}{}",
            to.scheme(),
            to.host_str().unwrap(),
            to.port().unwrap(),
            to.path()
        ),
        "http://127.0.0.1:49152/callback"
    );
    assert_eq!(param(&to, "state").as_deref(), Some(APP_STATE));
    assert_eq!(param(&to, "code").unwrap().len(), 43);
    assert!(param(&to, "error").is_none());
}

#[tokio::test]
async fn v6_sends_the_browser_to_the_ipv6_loopback() {
    let app = linked_app("sub-ada").await;
    let url = format!(
        "{}&loopback=v6",
        app.start_url(50000, APP_STATE, &challenge(VERIFIER))
    );
    let to = app.browse(&url).await.location();
    assert_eq!(to.host_str(), Some("[::1]"));
    assert_eq!(to.port(), Some(50000));
}

#[tokio::test]
async fn an_unlinked_subject_is_account_not_linked() {
    let app = linked_app("sub-ada").await;
    app.idp.set_identity(Identity::new("sub-stranger"));
    let to = app.sign_in_to_loopback().await;
    assert_eq!(param(&to, "error").as_deref(), Some("account_not_linked"));
    assert_eq!(param(&to, "state").as_deref(), Some(APP_STATE));
    assert!(param(&to, "code").is_none());
}

#[tokio::test]
async fn a_disabled_user_is_refused() {
    let app = linked_app("sub-ada").await;
    app.db
        .update_user(
            "ada",
            UserPatch::default().disabled(true),
            &AuditCtx::system(),
        )
        .await
        .unwrap();
    let to = app.sign_in_to_loopback().await;
    assert_eq!(param(&to, "error").as_deref(), Some("oidc_refused"));
}

#[tokio::test]
async fn a_bad_id_token_or_an_idp_error_is_refused() {
    let app = linked_app("sub-ada").await;
    app.idp.set_fault(Fault::WrongAudience);
    assert_eq!(
        param(&app.sign_in_to_loopback().await, "error").as_deref(),
        Some("oidc_refused")
    );
    app.idp.set_fault(Fault::None);
    let start = app
        .get(&app.start_url(49152, APP_STATE, &challenge(VERIFIER)))
        .await;
    let server_state = param(&start.location(), "state").unwrap();
    let hop = app
        .callback(&format!("error=access_denied&state={server_state}"))
        .await;
    assert_eq!(
        param(&hop.location(), "error").as_deref(),
        Some("oidc_refused")
    );
    assert_page(
        &app.callback(&format!("error=access_denied&state={server_state}"))
            .await,
        400,
    );
}

#[tokio::test]
async fn a_late_return_is_expired_and_a_later_one_starts_again() {
    let timings = Timings::new(
        Duration::from_millis(200),
        Duration::from_millis(600),
        Duration::from_secs(60),
    );
    let app = OidcApp::start(false, timings).await;
    app.idp.set_identity(Identity::new("sub-x"));
    let start = app
        .get(&app.start_url(49152, APP_STATE, &challenge(VERIFIER)))
        .await;
    let (code, state) = app.idp.authorize(start.location().as_str()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let hop = app.callback(&format!("code={code}&state={state}")).await;
    assert_eq!(
        param(&hop.location(), "error").as_deref(),
        Some("oidc_expired")
    );
    assert_eq!(param(&hop.location(), "state").as_deref(), Some(APP_STATE));

    let start = app
        .get(&app.start_url(49152, APP_STATE, &challenge(VERIFIER)))
        .await;
    let (code, state) = app.idp.authorize(start.location().as_str()).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_page(
        &app.callback(&format!("code={code}&state={state}")).await,
        400,
    );
}

#[tokio::test]
async fn an_unknown_state_is_a_start_again_page() {
    let app = linked_app("sub-ada").await;
    assert_page(&app.callback("code=x&state=never-issued").await, 400);
    assert_page(&app.callback("code=x").await, 400);
}
