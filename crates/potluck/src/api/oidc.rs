//! The OIDC sign-in routes under `/api/v1/oidc`. The browser sees HTML pages; with no provider
//! configured every route answers `not_found` like an unknown path.

use std::time::Instant;

use axum::Json;
use axum::extract::{FromRequest, Request, State};
use axum::http::header::{CACHE_CONTROL, LOCATION};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::AppState;
use super::client_ip::ClientIp;
use super::error::{ApiError, ApiResult};
use super::json::ApiJson;
use super::objects::{TokenJson, UserJson};
use super::oidc_pages::{BAD_START, IDP_UNAVAILABLE, START_AGAIN, page, too_many};
use super::session::SessionResponse;
use super::{tokens, validate};
use crate::clock;
use crate::db::accounts::TokenOrigin;
use crate::db::audit::AuditCtx;
use crate::metrics::{SignInMethod, SignInOutcome};
use crate::oidc::{Loopback, OidcState, PendingSignIn, Taken, random_secret};

/// How long a token minted by an OIDC sign-in lives: seven days.
const OIDC_TOKEN_SECS: i64 = 7 * 24 * 60 * 60;

/// The redeem body. No `Debug`, so the code and verifier cannot reach a log by formatting.
#[derive(Deserialize)]
struct RedeemRequest {
    code: String,
    verifier: String,
    token_name: String,
}

/// The app's loopback parameters from a start link.
struct StartParams {
    port: u16,
    state: String,
    challenge: String,
    loopback: Loopback,
}

/// `GET /api/v1/oidc/start?port&state&challenge&challenge_method=S256[&loopback=v4|v6]`:
/// 404 `not_found` JSON without OIDC; a 429 page past the address budget; a 422 page when a
/// parameter breaks its rule, repeats, or `redirect_uri` or `host` is present; a 503 page when
/// the provider cannot be reached; else the sign-in is stored and the answer is 302 to the IdP.
pub(super) async fn start(State(state): State<AppState>, ip: ClientIp, uri: Uri) -> Response {
    let Some(oidc) = state.oidc() else {
        return super::not_found().await.into_response();
    };
    if let Err(secs) = oidc.start_limiter().check(&ip.key()) {
        return too_many(secs);
    }
    let Some(params) = start_params(uri.query().unwrap_or("")) else {
        return page(StatusCode::UNPROCESSABLE_ENTITY, BAD_START);
    };

    // The IdP leg gets its own state, nonce and PKCE verifier, so nothing the start link carries
    // reaches the IdP.
    let now = Instant::now();
    let (verifier, nonce) = (random_secret(), random_secret());
    let entry = PendingSignIn::new(
        params.state,
        params.challenge,
        params.port,
        params.loopback,
        verifier.clone(),
        nonce.clone(),
    );
    let server_state = oidc.pending().insert_at(now, entry);
    match oidc
        .provider()
        .authorize_url_at(now, &redirect_uri(&state), &server_state, &nonce, &verifier)
        .await
    {
        Ok(url) => (StatusCode::FOUND, [(LOCATION, url)]).into_response(),
        Err(_) => {
            // The sign-in never reached the IdP, so its entry has no callback to wait for.
            oidc.pending().take_at(now, &server_state);
            page(StatusCode::SERVICE_UNAVAILABLE, IDP_UNAVAILABLE)
        }
    }
}

/// `GET /api/v1/oidc/callback?state&(code|error)`: 404 `not_found` JSON without OIDC; a 400
/// "start again" page when `state` names no sign-in in flight. Otherwise the sign-in is removed
/// and the browser goes 302 to the app's loopback with its `state` and either a handoff `code`
/// or `error`: `oidc_expired`, `oidc_refused` (IdP error, failed exchange or token, disabled
/// user) or `account_not_linked`.
pub(super) async fn callback(State(state): State<AppState>, uri: Uri) -> Response {
    let Some(oidc) = state.oidc() else {
        return super::not_found().await.into_response();
    };
    let pairs = query_pairs(uri.query().unwrap_or(""));
    let now = Instant::now();
    let taken = single(&pairs, "state")
        .flatten()
        .and_then(|server_state| oidc.pending().take_at(now, server_state));
    let entry = match taken {
        None => return page(StatusCode::BAD_REQUEST, START_AGAIN),
        Some(Taken::Expired(entry)) => return to_loopback(&entry, Err("oidc_expired")),
        Some(Taken::Fresh(entry)) => entry,
    };
    // The IdP's `error` wins over any `code` beside it; a repeated `code` counts as none.
    let code = match (single(&pairs, "error"), single(&pairs, "code")) {
        (Some(None), Some(Some(code))) => Some(code),
        _ => None,
    };
    let outcome = match code {
        Some(code) => finish(&state, oidc, &entry, code).await,
        None => Err("oidc_refused"),
    };
    to_loopback(&entry, outcome)
}

/// `POST /api/v1/oidc/redeem` `{code, verifier, token_name}`: 404 `not_found` without OIDC; 400
/// `invalid_request` for a body that does not parse; 422 `token_name`; 429 past the address
/// budget; 400 `oidc_code_invalid` when the code is unknown, used or late, the verifier's S256
/// does not match, or the user is gone or disabled; else 201 like `POST /session`, the token
/// expiring in seven days. The code is burned by its first try, right or wrong.
pub(super) async fn redeem(
    State(state): State<AppState>,
    ip: ClientIp,
    request: Request,
) -> ApiResult<(StatusCode, Json<SessionResponse>)> {
    // Without OIDC the route answers like an unknown path, whatever the body.
    let Some(oidc) = state.oidc() else {
        return Err(super::not_found().await);
    };
    let ApiJson(body) = ApiJson::<RedeemRequest>::from_request(request, &state).await?;
    let token_name = validate::token_name("token_name", &body.token_name)?;
    // Counted as in `POST /session`: the parse failure and the 422 count nothing.
    let count = |outcome| state.metrics().sign_in(SignInMethod::Oidc, outcome);
    oidc.redeem_limiter().check(&ip.key()).map_err(|e| {
        count(SignInOutcome::RateLimitedIp);
        ApiError::rate_limited(e)
    })?;
    let refuse = || {
        count(SignInOutcome::Failure);
        ApiError::oidc_code_invalid("the sign-in code is invalid or has expired")
    };

    let handoff = oidc
        .handoffs()
        .take_at(Instant::now(), &body.code)
        .ok_or_else(refuse)?;
    let derived = URL_SAFE_NO_PAD.encode(Sha256::digest(body.verifier.as_bytes()));
    if !bool::from(derived.as_bytes().ct_eq(handoff.challenge.as_bytes())) {
        return Err(refuse().into());
    }
    let user = match state.db().user_by_id(handoff.user_id).await? {
        Some(user) if !user.disabled() => user,
        _ => return Err(refuse().into()),
    };

    // The sign-in is the new token owner's act, not the anonymous request's.
    let ctx = AuditCtx::user(user.id(), user.username());
    let ctx = match ip.addr() {
        Some(addr) => ctx.with_client_ip(addr),
        None => ctx,
    };
    let expires_at = clock::shift_secs(&clock::now_rfc3339(), OIDC_TOKEN_SECS)
        .ok_or_else(|| ApiError::internal("could not compute the token expiry"))?;
    let (token, record) = tokens::mint_and_create(
        &state,
        user.id(),
        token_name,
        Some(expires_at.as_str()),
        TokenOrigin::Oidc,
        &ctx,
    )
    .await?;
    count(SignInOutcome::Success);
    Ok((
        StatusCode::CREATED,
        Json(SessionResponse {
            token,
            token_meta: TokenJson::from(&record),
            user: UserJson::from(&user),
        }),
    ))
}

/// Exchanges the IdP code and finds the linked user: a fresh handoff code for that user and the
/// app's challenge, or the loopback error code.
async fn finish(
    state: &AppState,
    oidc: &OidcState,
    entry: &PendingSignIn,
    code: &str,
) -> Result<String, &'static str> {
    let provider = oidc.provider();
    let claims = provider
        .sign_in_at(
            Instant::now(),
            &redirect_uri(state),
            code,
            &entry.verifier,
            &entry.nonce,
        )
        .await
        .map_err(|_| "oidc_refused")?;
    let issuer = provider.settings().issuer();
    let user = match state.db().user_by_oidc(issuer, claims.subject()).await {
        Ok(user) => user,
        Err(error) => {
            tracing::error!(%error, "OIDC sign-in could not read the account link");
            return Err("oidc_refused");
        }
    };
    match user {
        Some(user) if user.disabled() => Err("oidc_refused"),
        Some(user) => {
            Ok(oidc
                .handoffs()
                .insert_at(Instant::now(), user.id(), entry.challenge.clone()))
        }
        None => {
            tracing::info!(
                issuer,
                subject = claims.subject(),
                email = claims.email(),
                preferred_username = claims.preferred_username(),
                "OIDC sign-in by an identity linked to no account"
            );
            Err("account_not_linked")
        }
    }
}

/// 302 to the app's loopback listener with `code` or `error`, and the app's `state`, never
/// cached since it can carry a code. The URL is built from fixed parts and the stored port; no
/// request input shapes it.
fn to_loopback(entry: &PendingSignIn, outcome: Result<String, &'static str>) -> Response {
    let base = match entry.loopback {
        Loopback::V6 => "http://[::1]/callback",
        Loopback::V4 => "http://127.0.0.1/callback",
    };
    // Both bases are fixed valid URLs; the page only stands in for a panic.
    let Ok(mut url) = url::Url::parse(base) else {
        return page(StatusCode::BAD_REQUEST, START_AGAIN);
    };
    // An http URL with a host always takes a port.
    let _ = url.set_port(Some(entry.port));
    let (name, value) = match &outcome {
        Ok(code) => ("code", code.as_str()),
        Err(error) => ("error", *error),
    };
    url.query_pairs_mut()
        .append_pair(name, value)
        .append_pair("state", &entry.app_state);
    (
        StatusCode::FOUND,
        [
            (LOCATION, String::from(url)),
            (CACHE_CONTROL, "no-store".to_owned()),
        ],
    )
        .into_response()
}

/// The IdP redirect URI this server registered: `{public_url}/api/v1/oidc/callback`.
fn redirect_uri(state: &AppState) -> String {
    format!("{}/api/v1/oidc/callback", state.config().public_url())
}

/// A query string as its list of decoded pairs, order and repeats kept.
fn query_pairs(query: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

/// `None` when `name` repeats, else its single value if present.
fn single<'a>(pairs: &'a [(String, String)], name: &str) -> Option<Option<&'a str>> {
    let mut values = pairs
        .iter()
        .filter(|(k, _)| k == name)
        .map(|(_, v)| v.as_str());
    let first = values.next();
    values.next().is_none().then_some(first)
}

/// The start parameters when every rule holds. The query is read as a list so a repeated
/// parameter is refused rather than resolved; unknown parameters are ignored, except
/// `redirect_uri` and `host`, which the contract forbids.
fn start_params(query: &str) -> Option<StartParams> {
    let pairs = query_pairs(query);
    if pairs
        .iter()
        .any(|(k, _)| k == "redirect_uri" || k == "host")
    {
        return None;
    }
    let one = |name: &str| single(&pairs, name);
    let port = one("port")??.parse::<u16>().ok()?;
    let state = one("state")??;
    let challenge = one("challenge")??;
    let method = one("challenge_method")??;
    let loopback = match one("loopback")? {
        None | Some("v4") => Loopback::V4,
        Some("v6") => Loopback::V6,
        Some(_) => return None,
    };
    let ok = port >= 1024
        && (16..=64).contains(&state.len())
        && is_base64url(state)
        && challenge.len() == 43
        && is_base64url(challenge)
        && method == "S256";
    ok.then(|| StartParams {
        port,
        state: state.to_owned(),
        challenge: challenge.to_owned(),
        loopback,
    })
}

fn is_base64url(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
