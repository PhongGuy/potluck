//! `GET /api/v1/oidc/start`: the parameter rules, the redirect to the IdP, the pages.

use potluck::oidc::Timings;

use crate::common::idp::Fault;
use crate::common::oidc::{APP_STATE, OidcApp, VERIFIER, assert_page, challenge, param};
use crate::common::{call, config_for, migrated_db};

#[tokio::test]
async fn a_good_start_redirects_to_the_idp_with_server_values() {
    let app = OidcApp::start(false, Timings::default()).await;
    let hop = app
        .get(&app.start_url(49152, APP_STATE, &challenge(VERIFIER)))
        .await;
    assert_eq!(hop.status, 302, "{}", hop.body);
    let to = hop.location();
    assert!(
        to.as_str()
            .starts_with(&format!("{}/authorize", app.idp.issuer())),
        "{to}"
    );
    assert_eq!(
        param(&to, "redirect_uri").unwrap(),
        "http://127.0.0.1:8080/api/v1/oidc/callback"
    );
    assert_ne!(
        param(&to, "state").unwrap(),
        APP_STATE,
        "the app's state never reaches the IdP"
    );
    assert_ne!(param(&to, "code_challenge").unwrap(), challenge(VERIFIER));
    assert_eq!(param(&to, "prompt").as_deref(), Some("login"));
}

#[tokio::test]
async fn every_parameter_rule_is_a_422_page() {
    let app = OidcApp::start(false, Timings::default()).await;
    let c = challenge(VERIFIER);
    let good = format!("port=49152&state={APP_STATE}&challenge={c}&challenge_method=S256");
    let bad = [
        format!("port=1023&state={APP_STATE}&challenge={c}&challenge_method=S256"),
        format!("port=65536&state={APP_STATE}&challenge={c}&challenge_method=S256"),
        format!("port=abc&state={APP_STATE}&challenge={c}&challenge_method=S256"),
        format!("state={APP_STATE}&challenge={c}&challenge_method=S256"),
        format!("port=49152&state=short&challenge={c}&challenge_method=S256"),
        format!(
            "port=49152&state={}&challenge={c}&challenge_method=S256",
            "a".repeat(65)
        ),
        format!("port=49152&state=has.dot.0123456789&challenge={c}&challenge_method=S256"),
        format!(
            "port=49152&state={APP_STATE}&challenge={}&challenge_method=S256",
            &c[..42]
        ),
        format!("port=49152&state={APP_STATE}&challenge={c}&challenge_method=plain"),
        format!("port=49152&state={APP_STATE}&challenge={c}"),
        format!("{good}&loopback=v5"),
        format!("{good}&redirect_uri=http%3A%2F%2Fevil.example%2F"),
        format!("{good}&host=evil.example"),
        format!("{good}&port=2000"),
    ];
    for query in bad {
        let hop = app.get(&app.api(&format!("/oidc/start?{query}"))).await;
        assert_page(&hop, 422);
        assert!(
            !hop.body.contains(APP_STATE),
            "{query}: the page echoes nothing"
        );
    }
    for ok in [
        good.clone(),
        format!("{good}&loopback=v4"),
        format!("{good}&loopback=v6"),
        format!("{good}&extra=1"),
    ] {
        assert_eq!(
            app.get(&app.api(&format!("/oidc/start?{ok}"))).await.status,
            302,
            "{ok}"
        );
    }
}

#[tokio::test]
async fn an_unreachable_idp_is_a_503_page() {
    let app = OidcApp::start(false, Timings::default()).await;
    app.idp.set_fault(Fault::Unavailable);
    assert_page(
        &app.get(&app.start_url(49152, APP_STATE, &challenge(VERIFIER)))
            .await,
        503,
    );
}

#[tokio::test]
async fn start_is_rate_limited_on_its_own_budget() {
    let app = OidcApp::start_with_limits(false, Timings::default(), 10).await;
    let url = app.start_url(49152, APP_STATE, &challenge(VERIFIER));
    for _ in 0..10 {
        assert_eq!(app.get(&url).await.status, 302);
    }
    let hop = app.get(&url).await;
    assert_page(&hop, 429);
    assert!(hop.headers.contains_key("retry-after"));
    // The password budget is untouched: a wrong password still answers 401, not 429.
    let (status, _) = call(
        &app.api("/session"),
        reqwest::Method::POST,
        None,
        Some(serde_json::json!({"username": "nobody", "password": "x", "token_name": "t"})),
    )
    .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn without_oidc_the_routes_are_unknown() {
    let (dir, db) = migrated_db().await;
    let config = config_for(dir.path());
    let running =
        potluck::server::start_with(&config, potluck::api::AppState::new(db, config.clone()))
            .await
            .unwrap();
    let url = format!(
        "http://{}/api/v1/oidc/start?port=49152",
        running.http_addr()
    );
    let response = reqwest::get(url).await.unwrap();
    assert_eq!(response.status(), 404);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "not_found");
}
