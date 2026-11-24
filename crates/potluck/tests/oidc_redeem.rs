//! `POST /api/v1/oidc/redeem`: one code, one try, the POST /session shape and a 7-day token.

use std::time::Duration;

use potluck::api::AppState;
use potluck::db::accounts::UserPatch;
use potluck::db::audit::{AuditCtx, AuditFilter, AuditRecord};
use potluck::oidc::Timings;
use potluck::server::start_with;
use reqwest::Method;
use serde_json::{Value, json};

use crate::common::idp::Identity;
use crate::common::oidc::{OidcApp, VERIFIER, param};
use crate::common::{call, client, config_for, create_user, migrated_db, mint_token};
use crate::metrics::{sample, scrape};

async fn signed_in(app: &OidcApp) -> String {
    let ada = create_user(&app.db, "ada", "pw-123456", false).await;
    app.db
        .put_oidc_link(&app.idp.issuer(), ada.id(), "sub-ada", &AuditCtx::system())
        .await
        .unwrap();
    app.idp.set_identity(Identity::new("sub-ada"));
    param(&app.sign_in_to_loopback().await, "code").unwrap()
}

async fn redeem(app: &OidcApp, code: &str, verifier: &str) -> (u16, Value) {
    call(
        &app.api("/oidc/redeem"),
        Method::POST,
        None,
        Some(json!({"code": code, "verifier": verifier, "token_name": "laptop"})),
    )
    .await
}

fn stamp_secs(stamp: &str) -> i64 {
    stamp.parse::<jiff::Timestamp>().unwrap().as_second()
}

/// `action` rows written by `actor`, oldest first.
async fn rows_by(app: &OidcApp, action: &str, actor: &str) -> Vec<AuditRecord> {
    let mut rows = app
        .db
        .audit(AuditFilter::new().action(action).actor(actor).limit(1000))
        .await
        .unwrap();
    rows.reverse();
    rows
}

#[tokio::test]
async fn a_good_redeem_answers_like_post_session_with_a_seven_day_token() {
    let app = OidcApp::start(false, Timings::default()).await;
    let code = signed_in(&app).await;
    let (status, body) = redeem(&app, &code, VERIFIER).await;
    assert_eq!(status, 201, "{body}");
    let keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["token", "token_meta", "user"]);
    assert_eq!(body["user"]["username"], "ada");
    assert_eq!(body["token_meta"]["name"], "laptop");
    let life = stamp_secs(body["token_meta"]["expires_at"].as_str().unwrap())
        - stamp_secs(body["token_meta"]["created_at"].as_str().unwrap());
    // Two clock reads a second boundary apart may differ by one.
    assert!(
        (7 * 24 * 60 * 60 - 1..=7 * 24 * 60 * 60).contains(&life),
        "{life}"
    );
    let token = body["token"].as_str().unwrap();
    let (status, me) = call(&app.api("/me"), Method::GET, Some(token), None).await;
    assert_eq!(
        (status, me["user"]["username"].as_str()),
        (200, Some("ada"))
    );
}

#[tokio::test]
async fn a_code_is_burned_by_its_first_try_right_or_wrong() {
    let app = OidcApp::start(false, Timings::default()).await;
    let code = signed_in(&app).await;
    let (status, body) = redeem(&app, &code, "wrong-verifier-0123456789abcdefghijklmnop").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (400, Some("oidc_code_invalid"))
    );
    let (status, body) = redeem(&app, &code, VERIFIER).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (400, Some("oidc_code_invalid"))
    );
    let code = param(&app.sign_in_to_loopback().await, "code").unwrap();
    assert_eq!(redeem(&app, &code, VERIFIER).await.0, 201);
    assert_eq!(redeem(&app, &code, VERIFIER).await.0, 400, "replay");
    assert_eq!(redeem(&app, "never-issued", VERIFIER).await.0, 400);
}

#[tokio::test]
async fn a_code_expires() {
    let timings = Timings::new(
        Duration::from_secs(900),
        Duration::from_secs(1800),
        Duration::from_millis(200),
    );
    let app = OidcApp::start(false, timings).await;
    let code = signed_in(&app).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        redeem(&app, &code, VERIFIER).await.1["error"]["code"],
        "oidc_code_invalid"
    );
}

#[tokio::test]
async fn a_user_disabled_after_the_callback_gets_no_token() {
    let app = OidcApp::start(false, Timings::default()).await;
    let code = signed_in(&app).await;
    app.db
        .update_user(
            "ada",
            UserPatch::default().disabled(true),
            &AuditCtx::system(),
        )
        .await
        .unwrap();
    assert_eq!(
        redeem(&app, &code, VERIFIER).await.1["error"]["code"],
        "oidc_code_invalid"
    );
}

#[tokio::test]
async fn token_name_is_checked_before_the_code_is_taken() {
    let app = OidcApp::start(false, Timings::default()).await;
    let code = signed_in(&app).await;
    let (status, body) = call(
        &app.api("/oidc/redeem"),
        Method::POST,
        None,
        Some(json!({"code": code, "verifier": VERIFIER, "token_name": "  "})),
    )
    .await;
    assert_eq!(
        (status, body["error"]["field"].as_str()),
        (422, Some("token_name"))
    );
    assert_eq!(
        redeem(&app, &code, VERIFIER).await.0,
        201,
        "the code survived the 422"
    );
}

#[tokio::test]
async fn redeem_is_rate_limited() {
    let app = OidcApp::start_with_limits(false, Timings::default(), 10).await;
    for _ in 0..10 {
        assert_eq!(redeem(&app, "nope", VERIFIER).await.0, 400);
    }
    let (status, body) = redeem(&app, "nope", VERIFIER).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (429, Some("rate_limited"))
    );
}

#[tokio::test]
async fn the_sign_in_is_audited_as_an_oidc_session() {
    let app = OidcApp::start(false, Timings::default()).await;
    let code = signed_in(&app).await;
    let ada = app.db.user_by_username("ada").await.unwrap().unwrap();
    let token_rows = rows_by(&app, "token.create", "ada").await.len();
    let (status, body) = redeem(&app, &code, VERIFIER).await;
    assert_eq!(status, 201, "{body}");

    let row = rows_by(&app, "session.create", "ada").await.pop().unwrap();
    assert_eq!(
        (row.actor_kind(), row.actor_id(), row.actor_name()),
        ("user", Some(ada.id()), Some("ada"))
    );
    assert_eq!(row.client_ip(), Some("127.0.0.1"));
    assert_eq!(
        (row.target_kind(), row.target_id(), row.target_name()),
        (
            Some("token"),
            body["token_meta"]["id"].as_i64(),
            Some("laptop")
        )
    );
    assert_eq!(
        rows_by(&app, "token.create", "ada").await.len(),
        token_rows,
        "one row per minted token"
    );

    let root = create_user(&app.db, "root", "pw-123456", true).await;
    let admin = mint_token(&app.db, &root, "root", None).await;
    let (status, rows) = call(
        &app.api("/audit?action=session.create"),
        Method::GET,
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(rows[0]["actor_name"], "ada");
    assert_eq!(rows[0]["detail"]["method"], "oidc");
}

#[tokio::test]
async fn redeems_count_in_the_sign_in_metric_as_oidc() {
    let app = OidcApp::start_with_limits(false, Timings::default(), 2).await;
    let code = signed_in(&app).await;
    assert_eq!(redeem(&app, &code, VERIFIER).await.0, 201);
    assert_eq!(redeem(&app, &code, VERIFIER).await.0, 400);
    assert_eq!(redeem(&app, &code, VERIFIER).await.0, 429);
    let text = scrape(&app.running).await;
    let series = |o: &str| format!("potluck_sign_ins_total{{method=\"oidc\",outcome=\"{o}\"}}");
    assert_eq!(sample(&text, &series("success")), Some(1.0), "{text}");
    assert_eq!(sample(&text, &series("failure")), Some(1.0), "{text}");
    assert_eq!(
        sample(&text, &series("rate_limited_ip")),
        Some(1.0),
        "{text}"
    );
    assert!(!text.contains(&code), "the code leaked: {text}");
}

#[tokio::test]
async fn without_oidc_redeem_is_unknown_whatever_the_body() {
    let (dir, db) = migrated_db().await;
    let config = config_for(dir.path());
    let running = start_with(&config, AppState::new(db, config.clone()))
        .await
        .unwrap();
    let response = client()
        .post(format!("http://{}/api/v1/oidc/redeem", running.http_addr()))
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "not_found");
}
