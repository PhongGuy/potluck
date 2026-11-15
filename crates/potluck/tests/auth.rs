//! The `Authenticated` extractor over HTTP: anonymous, bearer, basic, system, and the 401 shape.

use std::net::SocketAddr;

use axum::routing::get;
use axum::{Extension, Router};
use base64::Engine;
use potluck::api::Actor;
use potluck::api::auth::Authenticated;
use potluck::api::error::ApiError;
use potluck::db::Db;
use serde_json::Value;

use crate::common::{client, create_user, mint_token, serve, test_state};

async fn whoami(auth: Authenticated) -> String {
    match auth.actor() {
        Actor::Anonymous => "anonymous".to_owned(),
        Actor::System => "system".to_owned(),
        Actor::User(u) => format!("user:{}:{}", u.user().username(), u.token_id()),
        _ => "other".to_owned(),
    }
}

async fn admin_only(auth: Authenticated) -> Result<&'static str, ApiError> {
    auth.require_server_admin()?;
    Ok("admin")
}

async fn self_only(auth: Authenticated) -> Result<String, ApiError> {
    Ok(auth.require_user()?.user().username().to_owned())
}

async fn app(actor: Option<Actor>) -> (tempfile::TempDir, Db, SocketAddr) {
    let (dir, db, state) = test_state().await;
    let mut router = Router::new()
        .route("/whoami", get(whoami))
        .route("/admin", get(admin_only))
        .route("/self", get(self_only));
    if let Some(actor) = actor {
        router = router.layer(Extension(actor));
    }
    let (addr, _task) = serve(router.with_state(state)).await;
    (dir, db, addr)
}

/// Status, body and the `WWW-Authenticate` header of `GET {path}` with `headers` (name, value).
async fn get_with(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, String, Option<String>) {
    let mut req = client().get(format!("http://{addr}{path}"));
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let r = req.send().await.unwrap();
    let www = r
        .headers()
        .get("www-authenticate")
        .map(|v| v.to_str().unwrap().to_owned());
    (r.status().as_u16(), r.text().await.unwrap(), www)
}

fn basic(user: &str, token: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"))
    )
}

fn error(body: &str) -> (String, String) {
    let v: Value = serde_json::from_str(body).unwrap();
    (
        v["error"]["code"].as_str().unwrap().to_owned(),
        v["error"]["message"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn no_header_is_anonymous_and_the_socket_actor_is_system() {
    let (_d, _db, addr) = app(None).await;
    assert_eq!(get_with(addr, "/whoami", &[]).await.1, "anonymous");
    let (_d, _db, addr) = app(Some(Actor::System)).await;
    let (status, body, _) =
        get_with(addr, "/whoami", &[("authorization", "Bearer plk_whatever")]).await;
    assert_eq!(
        (status, body.as_str()),
        (200, "system"),
        "the socket ignores headers"
    );
}

#[tokio::test]
async fn bearer_and_basic_resolve_to_the_same_user() {
    let (_d, db, addr) = app(None).await;
    let bob = create_user(&db, "bob", "pw", false).await;
    let secret = mint_token(&db, &bob, "laptop", None).await;
    let bearer = format!("Bearer {secret}");
    let (status, body, _) = get_with(addr, "/whoami", &[("authorization", bearer.as_str())]).await;
    assert_eq!(status, 200, "{body}");
    let id = db.tokens_for_user(bob.id()).await.unwrap()[0].id();
    assert_eq!(body, format!("user:bob:{id}"));
    let basic_header = basic("anyone", &secret);
    let (status, body2, _) =
        get_with(addr, "/whoami", &[("authorization", basic_header.as_str())]).await;
    assert_eq!(status, 200);
    assert_eq!(body2, body, "basic auth with any username is the same user");
}

#[tokio::test]
async fn two_authorization_headers_use_the_first() {
    let (_d, db, addr) = app(None).await;
    let bob = create_user(&db, "bob", "pw", false).await;
    let secret = mint_token(&db, &bob, "t", None).await;
    let good = format!("Bearer {secret}");
    let (status, body, _) = get_with(
        addr,
        "/whoami",
        &[
            ("authorization", good.as_str()),
            ("authorization", "Bearer plk_nope"),
        ],
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, _, _) = get_with(
        addr,
        "/whoami",
        &[
            ("authorization", "Bearer plk_nope"),
            ("authorization", good.as_str()),
        ],
    )
    .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn a_bad_credential_is_401_in_the_error_model() {
    let (_d, db, addr) = app(None).await;
    let bob = create_user(&db, "bob", "pw", false).await;
    let good = mint_token(&db, &bob, "ok", None).await;
    let mut unknown = good.clone();
    unknown.replace_range(40.., "zzzzzzz");
    let basic_unknown = basic("bob", &unknown);
    for (what, header, must_not_leak) in [
        ("unknown", format!("Bearer {unknown}"), &unknown[8..]),
        ("basic-unknown", basic_unknown.clone(), &unknown[8..]),
        ("malformed", "Bearer nope".to_owned(), "nope"),
        ("scheme", "Digest abc".to_owned(), "abc"),
        ("bare", "Bearer".to_owned(), "Bearer"),
    ] {
        let (status, body, www) =
            get_with(addr, "/whoami", &[("authorization", header.as_str())]).await;
        assert_eq!(status, 401, "{what}: {body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"]["code"], "unauthenticated", "{what}");
        assert_eq!(v["error"]["field"], Value::Null, "{what}");
        assert_eq!(www.as_deref(), Some("Basic realm=\"potluck\""), "{what}");
        assert!(
            !body.contains(must_not_leak),
            "{what}: the credential leaked into the body: {body}"
        );
    }
}

#[tokio::test]
async fn require_user_and_require_server_admin_answer_fixed_text() {
    let (_d, db, addr) = app(None).await;
    let bob = create_user(&db, "bob", "pw", false).await;
    let alice = create_user(&db, "alice", "pw", true).await;
    let bob_h = format!("Bearer {}", mint_token(&db, &bob, "t", None).await);
    let alice_h = format!("Bearer {}", mint_token(&db, &alice, "t", None).await);
    let (status, body, _) = get_with(addr, "/admin", &[]).await;
    assert_eq!(status, 401);
    assert_eq!(
        error(&body),
        (
            "unauthenticated".to_owned(),
            "a valid token is required".to_owned()
        )
    );
    let (status, body, _) = get_with(addr, "/admin", &[("authorization", bob_h.as_str())]).await;
    assert_eq!(status, 403);
    assert_eq!(
        error(&body),
        ("forbidden".to_owned(), "server admin only".to_owned())
    );
    assert_eq!(
        get_with(addr, "/admin", &[("authorization", alice_h.as_str())])
            .await
            .1,
        "admin"
    );
    assert_eq!(
        get_with(addr, "/self", &[("authorization", bob_h.as_str())])
            .await
            .1,
        "bob"
    );
    let (status, body, _) = get_with(addr, "/self", &[]).await;
    assert_eq!(status, 401);
    assert_eq!(error(&body).1, "a valid token is required");
    let (_d, _db, addr) = app(Some(Actor::System)).await;
    assert_eq!(
        get_with(addr, "/admin", &[]).await.1,
        "admin",
        "system is an admin"
    );
    let (status, body, _) = get_with(addr, "/self", &[]).await;
    assert_eq!(status, 403);
    assert_eq!(
        error(&body),
        (
            "forbidden".to_owned(),
            "the system actor has no account".to_owned()
        )
    );
}
