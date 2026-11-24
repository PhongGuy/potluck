//! One OpenID provider through openidconnect: discovery and its keys, the authorization URL for
//! a sign-in, and the code exchange with ID token verification. Only Potluck types cross out.

use std::time::{Duration, Instant};

use openidconnect::core::{
    CoreAuthPrompt, CoreClient, CoreJwsSigningAlgorithm, CoreProviderMetadata, CoreResponseType,
    CoreTokenResponse,
};
use openidconnect::{
    AuthenticationFlow, AuthorizationCode, ClaimsVerificationError, ClientId, CsrfToken,
    EndpointMaybeSet, EndpointNotSet, EndpointSet, HttpRequest, IssuerUrl, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};

use super::http::{IdpHttp, IdpHttpError};
use super::settings::OidcSettings;

/// Why a sign-in through the identity provider failed. The text names no detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// Discovery or the keys could not be fetched and nothing usable is cached.
    #[error("the identity provider is unavailable")]
    Unavailable,
    /// The code exchange failed or the ID token did not verify.
    #[error("the identity provider refused the sign-in")]
    Refused,
}

/// The verified claims of one ID token that Potluck uses.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct IdClaims {
    subject: String,
    email: Option<String>,
    preferred_username: Option<String>,
    name: Option<String>,
}

impl IdClaims {
    /// The `sub` claim: the user's stable id at the provider.
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// The `email` claim, if the provider sent one.
    pub fn email(&self) -> Option<&str> {
        self.email.as_deref()
    }

    /// The `preferred_username` claim, if the provider sent one.
    pub fn preferred_username(&self) -> Option<&str> {
        self.preferred_username.as_deref()
    }

    /// The `name` claim in its default language, if the provider sent one.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

// The client as openidconnect builds it from discovery: auth URL set, token URL maybe set.
type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

// One request in flight to the identity provider, as openidconnect's HTTP client returns it.
type IdpFuture = std::pin::Pin<
    Box<dyn Future<Output = Result<openidconnect::HttpResponse, IdpHttpError>> + Send>,
>;

// A cached copy older than this is refetched; a failed refetch keeps serving it.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
// No fetch starts this soon after a failed one, so a hanging IdP costs one timeout.
const FAILURE_PAUSE: Duration = Duration::from_secs(10);
// Server-wide spacing of refreshes forced by an ID token signed with an unknown key.
const FORCED_SPACING: Duration = Duration::from_secs(60);

// The discovery document with its keys, when it was fetched, and a counter that tells a
// sign-in whether the copy it verified against has since been replaced.
struct Cached {
    metadata: CoreProviderMetadata,
    fetched: Instant,
    generation: u64,
}

// The keys a forced refresh hands back, their generation, and whether this call fetched them
// or took a newer copy another caller had already fetched.
struct Refreshed {
    metadata: CoreProviderMetadata,
    generation: u64,
    fetched: bool,
}

// Everything behind the cache lock: the copy plus the instants that space out fetches.
#[derive(Default)]
struct Cache {
    cached: Option<Cached>,
    failed: Option<Instant>,
    forced: Option<Instant>,
}

/// One configured OpenID provider. Discovery and its keys are cached for a day; the cache lock
/// is held across a fetch so concurrent sign-ins wait for one fetch.
pub struct Provider {
    settings: OidcSettings,
    http: IdpHttp,
    cache: tokio::sync::Mutex<Cache>,
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fetched = self
            .cache
            .try_lock()
            .ok()
            .and_then(|c| c.cached.as_ref().map(|c| c.fetched));
        f.debug_struct("Provider")
            .field("settings", &self.settings)
            .field("fetched", &fetched)
            .finish_non_exhaustive()
    }
}

impl Provider {
    /// A provider for `settings` that sends every request through `http`. Nothing is fetched yet.
    pub fn new(settings: OidcSettings, http: IdpHttp) -> Provider {
        Provider {
            settings,
            http,
            cache: tokio::sync::Mutex::new(Cache::default()),
        }
    }

    /// Fetches discovery and keys once now; the startup probe. `Unavailable` when that fails.
    pub async fn warm(&self) -> Result<(), ProviderError> {
        let mut cache = self.cache.lock().await;
        self.fetch(&mut cache, Instant::now())
            .await
            .map_err(|_| unavailable("discovery"))
    }

    /// The settings this provider was built from.
    pub fn settings(&self) -> &OidcSettings {
        &self.settings
    }

    /// The IdP authorization URL for one sign-in: response_type=code, scopes
    /// `openid email profile`, `prompt=login`, `state`, `nonce`, and the S256 challenge of
    /// `pkce_verifier`, which must be 43 to 128 characters. `Unavailable` when discovery fails.
    pub async fn authorize_url_at(
        &self,
        now: Instant,
        redirect_uri: &str,
        state: &str,
        nonce: &str,
        pkce_verifier: &str,
    ) -> Result<String, ProviderError> {
        let (metadata, _) = self.metadata(now).await?;
        let client = self.client(metadata, redirect_uri)?;
        // openidconnect asserts the RFC 7636 length; refuse here rather than panic there.
        if !(43..=128).contains(&pkce_verifier.len()) {
            return Err(refused("pkce_verifier"));
        }
        let verifier = PkceCodeVerifier::new(pkce_verifier.to_owned());
        let (url, _, _) = client
            .authorize_url(
                AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
                {
                    let state = state.to_owned();
                    move || CsrfToken::new(state)
                },
                {
                    let nonce = nonce.to_owned();
                    move || Nonce::new(nonce)
                },
            )
            .add_scope(Scope::new("email".to_owned()))
            .add_scope(Scope::new("profile".to_owned()))
            .add_prompt(CoreAuthPrompt::Login)
            .set_pkce_challenge(PkceCodeChallenge::from_code_verifier_sha256(&verifier))
            .url();
        Ok(url.into())
    }

    /// Exchanges `code` with the client secret and `pkce_verifier`, then verifies the ID token
    /// (RS256 signature, iss, aud, exp, nonce). A signature failure verifies the same token
    /// against newer keys: a copy another caller cached since, and if that still fails, keys it
    /// fetches itself, unless a forced refresh started less than 60 s before `now`. The code is
    /// exchanged once and at most one fetch is made. `Unavailable` when discovery fails before
    /// the exchange, `Refused` when the exchange fails, the token does not verify, or a forced
    /// refresh fails.
    pub async fn sign_in_at(
        &self,
        now: Instant,
        redirect_uri: &str,
        code: &str,
        pkce_verifier: &str,
        nonce: &str,
    ) -> Result<IdClaims, ProviderError> {
        let (metadata, generation) = self.metadata(now).await?;
        let client = self.client(metadata, redirect_uri)?;
        let http = self.http_client();
        let response: CoreTokenResponse = client
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .map_err(|_| refused("exchange"))?
            .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier.to_owned()))
            .request_async(&http)
            .await
            .map_err(|_| refused("exchange"))?;
        let id_token = response.id_token().ok_or_else(|| refused("no_id_token"))?;
        // RS256 only, whatever discovery advertises: an HS256 token keyed by the client secret
        // must never pass.
        let nonce = Nonce::new(nonce.to_owned());
        let verify = |client: &Client| {
            let verifier = client
                .id_token_verifier()
                .set_allowed_algs([CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256]);
            id_token.claims(&verifier, &nonce).cloned()
        };
        // A signature failure may mean the IdP rotated its keys: verify against newer keys,
        // never repeating the code exchange. A newer cached copy may predate the rotation, so
        // when it also fails, refresh once more past it.
        let claims = match verify(&client) {
            Err(ClaimsVerificationError::SignatureVerification(_)) => {
                let newer = self.forced_refresh(now, generation).await?;
                let seen = newer.generation;
                match verify(&self.client(newer.metadata, redirect_uri)?) {
                    Err(ClaimsVerificationError::SignatureVerification(_)) if !newer.fetched => {
                        let fetched = self.forced_refresh(now, seen).await?;
                        verify(&self.client(fetched.metadata, redirect_uri)?)
                    }
                    other => other,
                }
            }
            other => other,
        }
        .map_err(|_| refused("claims"))?;
        Ok(IdClaims {
            subject: claims.subject().as_str().to_owned(),
            email: claims.email().map(|e| e.as_str().to_owned()),
            preferred_username: claims.preferred_username().map(|u| u.as_str().to_owned()),
            name: claims
                .name()
                .and_then(|n| n.get(None))
                .map(|n| n.as_str().to_owned()),
        })
    }

    // A client built from one discovery document, redirecting to `redirect_uri`.
    fn client(
        &self,
        metadata: CoreProviderMetadata,
        redirect_uri: &str,
    ) -> Result<Client, ProviderError> {
        let redirect =
            RedirectUrl::new(redirect_uri.to_owned()).map_err(|_| refused("redirect_uri"))?;
        let secret =
            openidconnect::ClientSecret::new(self.settings.client_secret().expose().to_owned());
        Ok(CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(self.settings.client_id().to_owned()),
            Some(secret),
        )
        .set_redirect_uri(redirect))
    }

    // The cached discovery document and its generation, refetched when it is a day old. A
    // failed refetch keeps the old copy; within the pause after a failure nothing is fetched.
    async fn metadata(&self, now: Instant) -> Result<(CoreProviderMetadata, u64), ProviderError> {
        let mut cache = self.cache.lock().await;
        let fresh = |c: &&Cached| now.saturating_duration_since(c.fetched) <= MAX_AGE;
        if let Some(cached) = cache.cached.as_ref().filter(fresh) {
            return Ok((cached.metadata.clone(), cached.generation));
        }
        if !paused(cache.failed, now, FAILURE_PAUSE) {
            match self.fetch(&mut cache, now).await {
                Ok(()) => {}
                Err(_) if cache.cached.is_some() => {
                    tracing::warn!("identity provider refresh failed; keeping the cached copy");
                }
                Err(_) => return Err(unavailable("discovery")),
            }
        }
        let cached = cache.cached.as_ref().ok_or(ProviderError::Unavailable)?;
        Ok((cached.metadata.clone(), cached.generation))
    }

    // Fresh keys after a signature failure. If the copy the sign-in used has since been
    // replaced, that newer copy is returned unfetched, though it may predate the token; otherwise
    // at most one forced fetch starts per minute server-wide, and none within a failure pause.
    async fn forced_refresh(&self, now: Instant, seen: u64) -> Result<Refreshed, ProviderError> {
        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache.cached.as_ref().filter(|c| c.generation != seen) {
            return Ok(Refreshed {
                metadata: cached.metadata.clone(),
                generation: cached.generation,
                fetched: false,
            });
        }
        if paused(cache.forced, now, FORCED_SPACING) || paused(cache.failed, now, FAILURE_PAUSE) {
            return Err(refused("refresh_spacing"));
        }
        cache.forced = Some(now);
        self.fetch(&mut cache, now)
            .await
            .map_err(|_| refused("refresh_failed"))?;
        let cached = cache.cached.as_ref().ok_or(ProviderError::Refused)?;
        Ok(Refreshed {
            metadata: cached.metadata.clone(),
            generation: cached.generation,
            fetched: true,
        })
    }

    // Fetches discovery and its keys into `cache`, recording the instant of a failure. It logs
    // nothing: each caller logs the failure once with its own reason.
    async fn fetch(&self, cache: &mut Cache, now: Instant) -> Result<(), ProviderError> {
        let issuer = IssuerUrl::new(self.settings.issuer().to_owned())
            .map_err(|_| ProviderError::Unavailable)?;
        match CoreProviderMetadata::discover_async(issuer, &self.http_client()).await {
            Ok(metadata) => {
                let generation = cache.cached.as_ref().map_or(0, |c| c.generation + 1);
                cache.failed = None;
                cache.cached = Some(Cached {
                    metadata,
                    fetched: now,
                    generation,
                });
                Ok(())
            }
            Err(_) => {
                cache.failed = Some(now);
                Err(ProviderError::Unavailable)
            }
        }
    }

    // The HTTP client handed to openidconnect: every request goes through `IdpHttp`.
    fn http_client(&self) -> impl Fn(HttpRequest) -> IdpFuture {
        let http = self.http.clone();
        move |request| {
            let http = http.clone();
            Box::pin(async move { http.call(request).await })
        }
    }
}

// Each failure is logged with a fixed reason only; the error text may carry IdP answers.
fn refused(reason: &'static str) -> ProviderError {
    tracing::warn!(reason, "identity provider refused the sign-in");
    ProviderError::Refused
}

fn unavailable(reason: &'static str) -> ProviderError {
    tracing::warn!(reason, "identity provider unavailable");
    ProviderError::Unavailable
}

// Whether `since` lies less than `pause` before `now`.
fn paused(since: Option<Instant>, now: Instant, pause: Duration) -> bool {
    since.is_some_and(|since| now.saturating_duration_since(since) < pause)
}
