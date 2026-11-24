//! The whole OIDC sign-in as an app sees it, and a scan of everything the server said for the
//! secrets that must never appear.

use potluck::db::audit::AuditCtx;
use potluck::oidc::Timings;
use reqwest::Method;
use serde_json::json;

use crate::common::idp::{CLIENT_SECRET, Identity};
use crate::common::oidc::{OidcApp, VERIFIER, param};
use crate::common::{call, create_user};

#[tokio::test]
async fn server_info_names_the_provider() {
    let app = OidcApp::start(false, Timings::default()).await;
    let (status, body) = call(&app.api("/server"), Method::GET, None, None).await;
    assert_eq!(status, 200);
    assert_eq!(
        body["auth"],
        json!({"password": true, "oidc": {"name": "127.0.0.1"}})
    );
}

#[tokio::test]
async fn nothing_the_server_answers_carries_a_secret() {
    let app = OidcApp::start(false, Timings::default()).await;
    let ada = create_user(&app.db, "ada", "pw-123456", false).await;
    app.db
        .put_oidc_link(&app.idp.issuer(), ada.id(), "sub-ada", &AuditCtx::system())
        .await
        .unwrap();
    app.idp.set_identity(Identity::new("sub-ada"));
    let to = app.sign_in_to_loopback().await;
    let code = param(&to, "code").unwrap();
    let (status, body) = call(
        &app.api("/oidc/redeem"),
        Method::POST,
        None,
        Some(json!({"code": code, "verifier": VERIFIER, "token_name": "t"})),
    )
    .await;
    assert_eq!(status, 201);
    let token = body["token"].as_str().unwrap().to_owned();
    let (_, replay) = call(
        &app.api("/oidc/redeem"),
        Method::POST,
        None,
        Some(json!({"code": code, "verifier": VERIFIER, "token_name": "t"})),
    )
    .await;
    let replay = replay.to_string();
    for secret in [CLIENT_SECRET, code.as_str(), VERIFIER, token.as_str()] {
        assert!(!replay.contains(secret), "{replay}");
    }
    assert!(!to.as_str().contains(CLIENT_SECRET));
}

#[test]
fn no_log_line_carries_a_secret() {
    let mut seen: Vec<String> = Vec::new();
    let text = crate::logs::captured_logs(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        seen = rt.block_on(async {
            let app = OidcApp::start(false, Timings::default()).await;
            let ada = create_user(&app.db, "ada", "pw-123456", false).await;
            app.db.put_oidc_link(&app.idp.issuer(), ada.id(), "sub-ada", &AuditCtx::system()).await.unwrap();
            app.idp.set_identity(Identity::new("sub-ada"));
            let code = param(&app.sign_in_to_loopback().await, "code").unwrap();
            let (_, wrong) = call(&app.api("/oidc/redeem"), Method::POST, None,
                Some(json!({"code": code, "verifier": "wrong-0123456789abcdefghijklmnopqrstuvw", "token_name": "t"}))).await;
            assert_eq!(wrong["error"]["code"], "oidc_code_invalid");
            let code = param(&app.sign_in_to_loopback().await, "code").unwrap();
            let (_, body) = call(&app.api("/oidc/redeem"), Method::POST, None,
                Some(json!({"code": code, "verifier": VERIFIER, "token_name": "t"}))).await;
            app.idp.set_identity(Identity::new("sub-stranger"));
            let _ = app.sign_in_to_loopback().await;
            vec![code, body["token"].as_str().unwrap().to_owned()]
        });
    });
    assert!(
        text.contains("sub-stranger"),
        "the unlinked subject is logged: {text}"
    );
    for secret in [CLIENT_SECRET, VERIFIER, "idp-code-", "idp-access-token"]
        .into_iter()
        .chain(seen.iter().map(String::as_str))
    {
        assert!(!text.contains(secret), "{secret} in logs: {text}");
    }
}
