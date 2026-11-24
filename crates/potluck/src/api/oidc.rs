//! The OIDC sign-in routes under `/api/v1/oidc`. The browser sees HTML pages; with no provider
//! configured every route answers `not_found` like an unknown path.

use std::time::Instant;

use axum::extract::State;
use axum::http::header::LOCATION;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use super::AppState;
use super::client_ip::ClientIp;
use super::oidc_pages::{BAD_START, IDP_UNAVAILABLE, page, too_many};
use crate::oidc::{Loopback, PendingSignIn, random_secret};

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
    let redirect_uri = format!("{}/api/v1/oidc/callback", state.config().public_url());
    match oidc
        .provider()
        .authorize_url_at(now, &redirect_uri, &server_state, &nonce, &verifier)
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

/// The start parameters when every rule holds. The query is read as a list so a repeated
/// parameter is refused rather than resolved; unknown parameters are ignored, except
/// `redirect_uri` and `host`, which the contract forbids.
fn start_params(query: &str) -> Option<StartParams> {
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    if pairs
        .iter()
        .any(|(k, _)| k == "redirect_uri" || k == "host")
    {
        return None;
    }
    // `None` when the parameter repeats, else its single value if present.
    let one = |name: &str| -> Option<Option<&str>> {
        let mut values = pairs
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.as_str());
        let first = values.next();
        values.next().is_none().then_some(first)
    };
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
