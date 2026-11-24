//! The `user link-oidc` and `user unlink-oidc` verbs over the admin socket.

use potluck::admin::AdminError;
use potluck::db::audit::AuditFilter;
use potluck::oidc::Timings;

use crate::common::admin;
use crate::common::create_user;
use crate::common::oidc::OidcApp;

#[tokio::test]
async fn link_then_unlink_through_the_cli() {
    let app = OidcApp::start(false, Timings::default()).await;
    let ada = create_user(&app.db, "ada", "pw-123456", false).await;
    let (result, out) = admin(app.dir.path(), &["user", "link-oidc", "ada", "sub-ada"], "").await;
    result.unwrap();
    assert_eq!(out, "linked ada to sub-ada\n");
    assert_eq!(
        app.db
            .oidc_link_of(&app.idp.issuer(), ada.id())
            .await
            .unwrap()
            .unwrap()
            .subject(),
        "sub-ada"
    );
    let (result, out) = admin(app.dir.path(), &["user", "unlink-oidc", "ada"], "").await;
    result.unwrap();
    assert_eq!(out, "unlinked ada\n");
    assert!(
        app.db
            .oidc_link_of(&app.idp.issuer(), ada.id())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn an_unknown_user_is_an_api_error() {
    let app = OidcApp::start(false, Timings::default()).await;
    let (result, _) = admin(app.dir.path(), &["user", "link-oidc", "nobody", "s"], "").await;
    let err = result.unwrap_err();
    assert!(
        matches!(&err, AdminError::Api { status: 404, .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn json_prints_the_link() {
    let app = OidcApp::start(false, Timings::default()).await;
    create_user(&app.db, "ada", "pw-123456", false).await;
    let (result, out) = admin(
        app.dir.path(),
        &["user", "link-oidc", "ada", "sub-ada", "--json"],
        "",
    )
    .await;
    result.unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["subject"], "sub-ada");
}

#[tokio::test]
async fn the_cli_link_and_unlink_are_audited_as_system() {
    let app = OidcApp::start(false, Timings::default()).await;
    create_user(&app.db, "ada", "pw-123456", false).await;
    let steps: [&[&str]; 2] = [
        &["user", "link-oidc", "ada", "sub-ada"],
        &["user", "unlink-oidc", "ada"],
    ];
    for args in steps {
        admin(app.dir.path(), args, "").await.0.unwrap();
    }
    // The socket's System actor carries no address, and both rows name the account and subject.
    for action in ["user.oidc_link", "user.oidc_unlink"] {
        let rows = app
            .db
            .audit(AuditFilter::new().action(action))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{action}");
        let row = &rows[0];
        assert_eq!(row.actor_kind(), "system", "{action}");
        assert_eq!(row.client_ip(), None, "{action}");
        assert_eq!(row.target_name(), Some("ada"), "{action}");
        assert_eq!(row.detail(), Some(r#"{"subject":"sub-ada"}"#), "{action}");
    }
}
