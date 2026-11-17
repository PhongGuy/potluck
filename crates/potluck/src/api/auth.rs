//! Turning a presented credential into the request's actor.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use super::credential::{Credential, credential_from_headers};
use super::error::{ApiError, ErrorResponse};
use super::{Actor, AppState, AuthUser};
use crate::auth::token::{digest, digests_match, prefix_of};
use crate::clock::{now_rfc3339, shift_secs};
use crate::db::Db;

/// Resolves `credential` to its user and token: a shape check, the rows sharing its prefix, a
/// constant-time digest match against every one, then expiry (`expires_at <= now`) and the
/// owner's `disabled` flag. On success `last_used_at` is refreshed when older than a minute.
/// Every refusal is 401 "invalid or expired token"; only a database failure answers otherwise.
pub async fn authenticate(state: &AppState, credential: &Credential) -> Result<AuthUser, ApiError> {
    authenticate_with(state.db(), credential).await
}

/// `authenticate` over the database handle alone: the same checks and the same 401s, the
/// disabled owner included.
pub async fn authenticate_with(db: &Db, credential: &Credential) -> Result<AuthUser, ApiError> {
    let secret = credential.secret();
    let Some(prefix) = prefix_of(secret) else {
        return Err(rejected("none", "malformed"));
    };
    let presented = digest(secret);
    let candidates = db.tokens_by_prefix(prefix).await?;
    // Compare against every candidate before choosing, so the time taken does not depend on
    // which row (if any) matched.
    let mut found = None;
    for candidate in candidates {
        if digests_match(candidate.digest(), &presented) && found.is_none() {
            found = Some(candidate);
        }
    }
    let Some(candidate) = found else {
        return Err(rejected(prefix, "unknown"));
    };
    let now = now_rfc3339();
    if candidate
        .token()
        .expires_at()
        .is_some_and(|at| at <= now.as_str())
    {
        return Err(rejected(prefix, "expired"));
    }
    // A disabled owner is refused here, before any `Actor::User` exists to carry their flags.
    if candidate.user().disabled() {
        return Err(rejected(prefix, "disabled"));
    }
    let token_id = candidate.token().id();
    // `last_used_at` is advisory: the request is already authenticated, so a failed touch is
    // reported and ignored.
    if let Some(stale_before) = shift_secs(&now, -60)
        && let Err(err) = db.touch_token(token_id, &now, &stale_before).await
    {
        tracing::warn!(error = %err, "could not record token use");
    }
    Ok(AuthUser::new(candidate.user().clone(), token_id))
}

// The one refusal every bad token gets; the log names only the displayable prefix and why.
fn rejected(prefix: &str, reason: &'static str) -> ApiError {
    tracing::debug!(prefix, reason, "token rejected");
    ApiError::unauthenticated("invalid or expired token")
}

/// The request's actor: `System` when the `Extension<Actor>` says so (the admin socket), else
/// the `Authorization` header resolved through `authenticate`, `Anonymous` without one, 401 for
/// a present but unusable or unknown credential. A missing extension counts as `Anonymous`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct Authenticated {
    actor: Actor,
}

impl Authenticated {
    /// The resolved actor.
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// The signed-in user: `Anonymous` is 401 "a valid token is required"; `System` is 403
    /// "the system actor has no account".
    pub fn require_user(&self) -> Result<&AuthUser, ApiError> {
        match &self.actor {
            Actor::User(user) => Ok(user),
            Actor::System => Err(ApiError::forbidden("the system actor has no account")),
            Actor::Anonymous => Err(token_required()),
        }
    }

    /// `Anonymous` is 401 "a valid token is required"; a user without `server_admin` is 403
    /// "server admin only"; `System` passes.
    pub fn require_server_admin(&self) -> Result<(), ApiError> {
        match &self.actor {
            Actor::Anonymous => Err(token_required()),
            actor if actor.is_server_admin() => Ok(()),
            _ => Err(ApiError::forbidden("server admin only")),
        }
    }
}

pub(super) fn token_required() -> ApiError {
    ApiError::unauthenticated("a valid token is required")
}

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = ErrorResponse;

    // The admin socket's actor is trusted outright, so its headers are never parsed; on TCP the
    // header decides, and only its absence is anonymous.
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, ErrorResponse> {
        if let Some(Actor::System) = parts.extensions.get::<Actor>() {
            return Ok(Authenticated {
                actor: Actor::System,
            });
        }
        let actor = match credential_from_headers(&parts.headers)? {
            Some(credential) => Actor::User(authenticate(state, &credential).await?),
            None => Actor::Anonymous,
        };
        Ok(Authenticated { actor })
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::routing::get;
    use axum::{Extension, Router};
    use base64::Engine;
    use serde_json::Value;

    use super::*;
    use crate::api::error::ApiResult;
    use crate::api::test_support::{create_user, mint_token, serve, test_state};
    use crate::api::{Actor, AuthUser};
    use crate::db::Db;

    async fn whoami(auth: Authenticated) -> String {
        match auth.actor() {
            Actor::Anonymous => "anonymous".to_owned(),
            Actor::System => "system".to_owned(),
            Actor::User(u) => format!("user:{}:{}", u.user().username(), u.token_id()),
        }
    }

    async fn admin_only(auth: Authenticated) -> ApiResult<&'static str> {
        auth.require_server_admin()?;
        Ok("admin")
    }

    async fn self_only(auth: Authenticated) -> ApiResult<String> {
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
        let mut req = reqwest::Client::new().get(format!("http://{addr}{path}"));
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
        let (status, body, _) =
            get_with(addr, "/whoami", &[("authorization", bearer.as_str())]).await;
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
        let (status, body, _) =
            get_with(addr, "/admin", &[("authorization", bob_h.as_str())]).await;
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

    #[tokio::test]
    async fn an_anonymous_extension_still_lets_the_header_decide() {
        let (_d, db, addr) = app(Some(Actor::Anonymous)).await;
        let bob = create_user(&db, "bob", "pw", false).await;
        let bearer = format!("Bearer {}", mint_token(&db, &bob, "t", None).await);
        let (status, body, _) =
            get_with(addr, "/whoami", &[("authorization", bearer.as_str())]).await;
        let id = db.tokens_for_user(bob.id()).await.unwrap()[0].id();
        assert_eq!((status, body), (200, format!("user:bob:{id}")));
        let (status, _, _) =
            get_with(addr, "/whoami", &[("authorization", "Bearer plk_nope")]).await;
        assert_eq!(status, 401, "a bad token is still refused");
        assert_eq!(get_with(addr, "/whoami", &[]).await.1, "anonymous");
    }

    #[tokio::test]
    async fn a_user_extension_is_never_trusted() {
        // Only `System` is taken from the extension; a planted `User` must not sign anyone in.
        let (_dir, db, state) = test_state().await;
        let alice = create_user(&db, "alice", "pw", true).await;
        let bob = create_user(&db, "bob", "pw", false).await;
        let router = Router::new()
            .route("/whoami", get(whoami))
            .layer(Extension(Actor::User(AuthUser::new(alice, 99))));
        let (addr, _task) = serve(router.with_state(state)).await;
        assert_eq!(get_with(addr, "/whoami", &[]).await.1, "anonymous");
        let bearer = format!("Bearer {}", mint_token(&db, &bob, "t", None).await);
        let id = db.tokens_for_user(bob.id()).await.unwrap()[0].id();
        let (status, body, _) =
            get_with(addr, "/whoami", &[("authorization", bearer.as_str())]).await;
        assert_eq!((status, body), (200, format!("user:bob:{id}")));
    }

    // `<T as Probe<_>>` is ambiguous, and fails to compile, exactly when `T: PartialEq`.
    trait Probe<A> {
        fn not_partial_eq() {}
    }
    impl<T: ?Sized> Probe<()> for T {}
    struct ImplementsPartialEq;
    impl<T: ?Sized + PartialEq> Probe<ImplementsPartialEq> for T {}

    #[test]
    fn authenticated_does_not_implement_partial_eq() {
        <Authenticated as Probe<_>>::not_partial_eq();
    }
}
