//! The update checker: fetches the signed manifest and its signature from the source, verifies
//! them, and keeps the newest verified release in memory for the status readers.

use std::sync::{Arc, Mutex, PoisonError};

use reqwest::StatusCode;
use reqwest::redirect::Policy;
use semver::Version;
use url::Url;

use super::{
    MANIFEST_FILE, MAX_FILE_BYTES, Release, SIGNATURE_FILE, UpdateError, UpdateSource,
    verify_release,
};
use crate::clock;

/// Redirects followed per request; more is refused.
const MAX_REDIRECTS: usize = 5;

/// What the checker knows: the running version, the newest verified release above it, and when
/// a verified answer last arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UpdateStatus {
    current: String,
    latest: Option<Release>,
    checked_at: Option<String>,
}

impl UpdateStatus {
    /// The running version.
    pub fn current(&self) -> &str {
        &self.current
    }

    /// The highest verified final release newer than the running version, if any.
    pub fn latest(&self) -> Option<&Release> {
        self.latest.as_ref()
    }

    /// When the last verified, well-formed answer arrived, as RFC 3339 UTC.
    pub fn checked_at(&self) -> Option<&str> {
        self.checked_at.as_deref()
    }
}

/// Checks a source for releases newer than the running version. Clones share one state.
#[derive(Clone)]
pub struct UpdateChecker {
    inner: Arc<Inner>,
}

struct Inner {
    source: UpdateSource,
    current: Version,
    state: Mutex<UpdateStatus>,
}

impl UpdateChecker {
    /// A checker for `source`, comparing against `current`; `CurrentVersion` when `current` is
    /// not semver.
    pub fn new(source: UpdateSource, current: &str) -> Result<UpdateChecker, UpdateError> {
        let version = Version::parse(current).map_err(|_| UpdateError::CurrentVersion {
            version: current.to_owned(),
        })?;
        let state = UpdateStatus {
            current: current.to_owned(),
            latest: None,
            checked_at: None,
        };
        Ok(UpdateChecker {
            inner: Arc::new(Inner {
                source,
                current: version,
                state: Mutex::new(state),
            }),
        })
    }

    /// Fetches and verifies the signed release, records it when it is newer than the running
    /// version, and returns the resulting status. A failure leaves the status unchanged.
    pub async fn check(&self) -> Result<UpdateStatus, UpdateError> {
        let inner = &self.inner;
        let client = build_client(&inner.source, &inner.current.to_string())?;
        let manifest = fetch(&client, &inner.source.base, MANIFEST_FILE).await?;
        let signature = fetch(&client, &inner.source.base, SIGNATURE_FILE).await?;
        let release = verify_release(&inner.source.keys, &manifest, &signature)?;
        let version = Version::parse(release.version())
            .map_err(|_| UpdateError::Manifest { field: "version" })?;
        let newer = version.cmp_precedence(&inner.current).is_gt();
        // The guard is taken only after the last await, so the check future stays Send.
        let mut state = inner.lock();
        if newer {
            state.latest = Some(release);
        }
        state.checked_at = Some(clock::now_rfc3339());
        Ok(state.clone())
    }

    /// The status as of the last check.
    pub fn status(&self) -> UpdateStatus {
        self.inner.lock().clone()
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, UpdateStatus> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

// A fresh client per check, so a host without CA certificates fails that check as Network
// rather than the server's startup. Loopback http bases (tests) bypass any configured proxy.
fn build_client(source: &UpdateSource, current: &str) -> Result<reqwest::Client, UpdateError> {
    let https = source.base.scheme() == "https";
    let policy = Policy::custom(|attempt| {
        if redirect_allowed(attempt.url(), attempt.previous().len()) {
            attempt.follow()
        } else {
            attempt.error("redirect refused")
        }
    });
    let mut builder = reqwest::Client::builder()
        .user_agent(format!("potluck/{current}"))
        .redirect(policy)
        .connect_timeout(source.connect)
        .timeout(source.total)
        .https_only(https);
    if !https {
        builder = builder.no_proxy();
    }
    builder.build().map_err(|_| UpdateError::Network)
}

// `hop` counts the URLs already visited, the initial one included, so the first redirect is 1.
fn redirect_allowed(next: &Url, hop: usize) -> bool {
    next.scheme() == "https" && hop <= MAX_REDIRECTS
}

// Reads one release file, capped at MAX_FILE_BYTES chunk by chunk since reqwest has no limit.
// reqwest errors are classed and dropped, never formatted, so no fetched text leaks out.
async fn fetch(client: &reqwest::Client, base: &Url, file: &str) -> Result<Vec<u8>, UpdateError> {
    let url = base.join(file).map_err(|_| UpdateError::BaseUrl {
        reason: "a release file does not resolve against it".into(),
    })?;
    let mut response = client.get(url).send().await.map_err(transport)?;
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Err(UpdateError::NotFound);
    }
    if !status.is_success() {
        return Err(UpdateError::Status {
            status: status.as_u16(),
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport)? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_FILE_BYTES {
            return Err(UpdateError::TooLarge);
        }
    }
    Ok(body)
}

fn transport(err: reqwest::Error) -> UpdateError {
    if err.is_redirect() {
        UpdateError::Redirect
    } else {
        UpdateError::Network
    }
}

#[cfg(test)]
mod tests {
    use super::redirect_allowed;

    #[test]
    fn redirects_follow_https_only_and_at_most_five_hops() {
        let https = url::Url::parse("https://objects.example.com/x").unwrap();
        let http = url::Url::parse("http://objects.example.com/x").unwrap();
        assert!(redirect_allowed(&https, 1), "the first hop");
        assert!(redirect_allowed(&https, 5), "the fifth hop");
        assert!(!redirect_allowed(&https, 6), "a sixth hop is too many");
        assert!(!redirect_allowed(&http, 1), "never to http");
    }
}
