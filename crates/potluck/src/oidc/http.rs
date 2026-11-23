//! The HTTP client every identity provider request goes through: no redirects, capped
//! timeouts and a capped body, and errors that carry neither the URL nor the answer.

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use reqwest::redirect::Policy;

/// Connect timeout for an identity provider request.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Total time for one identity provider request, body included.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest identity provider answer read.
const MAX_BODY: usize = 1024 * 1024;

/// Why a request to the identity provider failed. The text never names the URL or the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdpHttpError {
    /// The connection failed, or the client could not be built.
    #[error("the identity provider could not be reached")]
    Transport,
    /// The connect or total timeout ran out.
    #[error("the identity provider did not answer in time")]
    Timeout,
    /// The answer was a 3xx, which is never followed.
    #[error("the identity provider answered with a redirect")]
    Redirect,
    /// The body passed the cap.
    #[error("the identity provider's answer is too large")]
    TooLarge,
}

/// An HTTP client for one identity provider. Clones share one connection pool.
#[derive(Clone)]
pub struct IdpHttp {
    client: reqwest::Client,
    max_body: usize,
}

impl IdpHttp {
    /// A client with a 5 s connect timeout, a 10 s total timeout and a 1 MiB body cap;
    /// `https_only` for an https issuer.
    pub fn new(https_only: bool) -> Result<IdpHttp, IdpHttpError> {
        IdpHttp::with_limits(https_only, CONNECT_TIMEOUT, TOTAL_TIMEOUT, MAX_BODY)
    }

    /// A client with the given timeouts and body cap.
    pub fn with_limits(
        https_only: bool,
        connect: Duration,
        total: Duration,
        max_body: usize,
    ) -> Result<IdpHttp, IdpHttpError> {
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("potluck/", env!("CARGO_PKG_VERSION")))
            .redirect(Policy::none())
            .connect_timeout(connect)
            .timeout(total)
            .https_only(https_only);
        // A plain http issuer is a local one; an ambient proxy must not intercept it.
        if !https_only {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(|_| IdpHttpError::Transport)?;
        Ok(IdpHttp { client, max_body })
    }

    /// GET `url`: the status and the whole body; any 3xx is `Redirect`.
    pub async fn get(&self, url: &str) -> Result<(u16, Vec<u8>), IdpHttpError> {
        let (status, _, body) = self.send(self.client.get(url)).await?;
        Ok((status.as_u16(), body))
    }

    /// Sends one request openidconnect built and hands back its answer; any 3xx is `Redirect`.
    /// Crate-private because its types come from the `http` crate.
    pub(crate) async fn call(
        &self,
        request: openidconnect::HttpRequest,
    ) -> Result<openidconnect::HttpResponse, IdpHttpError> {
        let (parts, body) = request.into_parts();
        let builder = self
            .client
            .request(parts.method, parts.uri.to_string())
            .headers(parts.headers)
            .body(body);
        let (status, headers, body) = self.send(builder).await?;
        let mut response = openidconnect::HttpResponse::new(body);
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        Ok(response)
    }

    // Sends one request and reads its body chunk by chunk under the cap, since reqwest has no
    // limit of its own. With redirects off a 3xx arrives as a response and is refused here.
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<(StatusCode, HeaderMap, Vec<u8>), IdpHttpError> {
        let mut response = request.send().await.map_err(classify)?;
        let status = response.status();
        if status.is_redirection() {
            return Err(IdpHttpError::Redirect);
        }
        let headers = std::mem::take(response.headers_mut());
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(classify)? {
            if body.len() + chunk.len() > self.max_body {
                return Err(IdpHttpError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, headers, body))
    }
}

// reqwest errors are classed and dropped, never formatted, so no URL or answer leaks out.
fn classify(err: reqwest::Error) -> IdpHttpError {
    if err.is_timeout() {
        IdpHttpError::Timeout
    } else {
        IdpHttpError::Transport
    }
}
