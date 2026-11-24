//! Just-in-time accounts at the OIDC callback.

use potluck::oidc::Timings;
use reqwest::Method;
use serde_json::json;

use potluck::db::audit::{AuditCtx, AuditFilter};

use crate::common::idp::Identity;
use crate::common::oidc::{OidcApp, VERIFIER, param};
use crate::common::{call, create_user};

fn who(subject: &str, username: Option<&str>, email: Option<&str>) -> Identity {
    Identity {
        subject: subject.into(),
        email: email.map(Into::into),
        preferred_username: username.map(Into::into),
        name: Some("Grace H".into()),
    }
}

#[tokio::test]
async fn jit_creates_a_linked_account_that_can_redeem() {
    let app = OidcApp::start(true, Timings::default()).await;
    app.idp
        .set_identity(who("sub-grace", Some("grace"), Some("grace@example.com")));
    let code = param(&app.sign_in_to_loopback().await, "code").expect("a code, not an error");
    let (status, body) = call(
        &app.api("/oidc/redeem"),
        Method::POST,
        None,
        Some(json!({"code": code, "verifier": VERIFIER, "token_name": "t"})),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["user"]["username"], "grace");
    assert_eq!(body["user"]["display_name"], "Grace H");
    assert_eq!(body["user"]["email"], "grace@example.com");
    assert_eq!(body["user"]["server_admin"], false);
    let again = param(&app.sign_in_to_loopback().await, "code");
    assert!(again.is_some(), "the second sign-in uses the link");
}

// Runs inside the log capture, which needs a current-thread runtime on the test's own thread.
#[test]
fn jit_is_off_by_default() {
    let text = crate::logs::captured_logs(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let app = OidcApp::start(false, Timings::default()).await;
            app.idp.set_identity(who("sub-grace", Some("grace"), None));
            let to = app.sign_in_to_loopback().await;
            assert_eq!(param(&to, "error").as_deref(), Some("account_not_linked"));
            assert!(app.db.user_by_username("grace").await.unwrap().is_none());
        });
    });
    let line = text
        .lines()
        .find(|l| l.contains("linked to no account"))
        .unwrap_or_else(|| panic!("the account_not_linked info line: {text}"));
    assert!(
        line.contains("sub-grace") && line.contains("grace"),
        "{line}"
    );
}

#[tokio::test]
async fn jit_never_takes_over_an_existing_user() {
    let app = OidcApp::start(true, Timings::default()).await;
    let ada = create_user(&app.db, "ada", "pw-123456", false).await;
    app.db
        .update_user(
            "ada",
            potluck::db::accounts::UserPatch::default().email(Some("ada@example.com")),
            &AuditCtx::system(),
        )
        .await
        .unwrap();
    app.idp.set_identity(who("sub-evil", Some("ada"), None));
    assert_eq!(
        param(&app.sign_in_to_loopback().await, "error").as_deref(),
        Some("account_not_linked")
    );
    app.idp.set_identity(who(
        "sub-evil",
        Some("someone-new"),
        Some("ada@example.com"),
    ));
    let to = app.sign_in_to_loopback().await;
    assert!(param(&to, "code").is_some(), "a new account, not ada's");
    assert!(
        app.db
            .oidc_link_of(&app.idp.issuer(), ada.id())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn unusable_name_and_email_claims_fall_back() {
    let app = OidcApp::start(true, Timings::default()).await;
    for (i, (name, email)) in [("", "not-an-email"), (&"x".repeat(201)[..], "")]
        .into_iter()
        .enumerate()
    {
        let username = format!("user-{i}");
        app.idp.set_identity(Identity {
            subject: format!("sub-{i}"),
            email: Some(email.into()),
            preferred_username: Some(username.clone()),
            name: Some(name.into()),
        });
        assert!(param(&app.sign_in_to_loopback().await, "code").is_some());
        let user = app.db.user_by_username(&username).await.unwrap().unwrap();
        assert_eq!(user.display_name(), username);
        assert_eq!(user.email(), None);
    }
}

#[tokio::test]
async fn unusable_names_are_not_linked() {
    let app = OidcApp::start(true, Timings::default()).await;
    for (i, name) in [
        None,
        Some("Grace Hopper"),
        Some("system"),
        Some("-dash"),
        Some("UPPER"),
    ]
    .into_iter()
    .enumerate()
    {
        app.idp.set_identity(who(&format!("sub-{i}"), name, None));
        assert_eq!(
            param(&app.sign_in_to_loopback().await, "error").as_deref(),
            Some("account_not_linked"),
            "{name:?}"
        );
    }
}

#[tokio::test]
async fn a_jit_sign_in_is_audited_as_the_new_user() {
    let app = OidcApp::start(true, Timings::default()).await;
    app.idp.set_identity(who("sub-grace", Some("grace"), None));
    assert!(param(&app.sign_in_to_loopback().await, "code").is_some());
    for action in ["user.create", "user.oidc_link"] {
        let rows = app
            .db
            .audit(AuditFilter::new().action(action))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{action}");
        let row = &rows[0];
        assert_eq!(row.actor_kind(), "user", "{action}");
        assert_eq!(row.actor_name(), Some("grace"), "{action}");
        assert_eq!(row.client_ip(), Some("127.0.0.1"), "{action}");
        assert_eq!(row.target_name(), Some("grace"), "{action}");
    }

    app.idp
        .set_identity(who("sub-system", Some("system"), None));
    assert_eq!(
        param(&app.sign_in_to_loopback().await, "error").as_deref(),
        Some("account_not_linked")
    );
    let creates = app
        .db
        .audit(AuditFilter::new().action("user.create"))
        .await
        .unwrap();
    assert_eq!(creates.len(), 1, "a refused JIT writes no user.create row");
}
