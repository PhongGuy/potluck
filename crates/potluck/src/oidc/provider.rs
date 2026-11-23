//! One OpenID provider through openidconnect: discovery and its keys, the authorization URL for
//! a sign-in, and the code exchange with ID token verification. Only Potluck types cross out.

use std::time::Instant;

use openidconnect::core::{
    CoreAuthPrompt, CoreClient, CoreJwsSigningAlgorithm, CoreProviderMetadata, CoreResponseType,
    CoreTokenResponse,
};
use openidconnect::{
    AuthenticationFlow, AuthorizationCode, ClientId, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, HttpRequest, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl,
    Scope, TokenResponse,
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

// The discovery document with its keys, and when it was fetched.
struct Cached {
    metadata: CoreProviderMetadata,
    fetched: Instant,
}

/// One configured OpenID provider. Discovery and its keys are fetched once and cached; the
/// cache lock is held across a fetch so concurrent sign-ins wait for one fetch.
pub struct Provider {
    settings: OidcSettings,
    http: IdpHttp,
    cache: tokio::sync::Mutex<Option<Cached>>,
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fetched = self
            .cache
            .try_lock()
            .ok()
            .and_then(|c| c.as_ref().map(|c| c.fetched));
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
            cache: tokio::sync::Mutex::new(None),
        }
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
        let client = self.client(now, redirect_uri).await?;
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
    /// (RS256 signature, iss, aud, exp, nonce). `Unavailable` when discovery fails, `Refused`
    /// when the exchange fails or the token does not verify.
    pub async fn sign_in_at(
        &self,
        now: Instant,
        redirect_uri: &str,
        code: &str,
        pkce_verifier: &str,
        nonce: &str,
    ) -> Result<IdClaims, ProviderError> {
        let client = self.client(now, redirect_uri).await?;
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
        let verifier = client
            .id_token_verifier()
            .set_allowed_algs([CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256]);
        let claims = id_token
            .claims(&verifier, &Nonce::new(nonce.to_owned()))
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

    // A client built from the cached discovery document, fetching it first if nothing is cached.
    async fn client(&self, now: Instant, redirect_uri: &str) -> Result<Client, ProviderError> {
        let metadata = self.metadata(now).await?;
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

    // The cached discovery document, or a fresh one. The lock is held across the fetch.
    async fn metadata(&self, now: Instant) -> Result<CoreProviderMetadata, ProviderError> {
        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache.as_ref() {
            return Ok(cached.metadata.clone());
        }
        let issuer = IssuerUrl::new(self.settings.issuer().to_owned())
            .map_err(|_| unavailable("discovery"))?;
        let metadata = CoreProviderMetadata::discover_async(issuer, &self.http_client())
            .await
            .map_err(|_| unavailable("discovery"))?;
        *cache = Some(Cached {
            metadata: metadata.clone(),
            fetched: now,
        });
        Ok(metadata)
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
