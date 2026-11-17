//! The update checker: fetches the signed manifest and its signature from the source, verifies
//! them, and keeps the newest verified release in memory for the status readers.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use reqwest::StatusCode;
use reqwest::redirect::Policy;
use semver::Version;
use tokio::sync::watch;
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
    inflight: SingleFlight<Result<UpdateStatus, UpdateError>>,
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
                inflight: SingleFlight::new(),
            }),
        })
    }

    /// Fetches and verifies the signed release, records it when it is a final release above both
    /// the running version and the current `latest`, and returns the resulting status. A failure
    /// leaves the status unchanged. A call while a check runs joins it and shares its answer;
    /// the check runs in its own task, so a caller that gives up does not cancel it, and one
    /// that panics answers `Interrupted`. Each check logs exactly one line.
    pub async fn check(&self) -> Result<UpdateStatus, UpdateError> {
        let inner = Arc::clone(&self.inner);
        let answer = self
            .inner
            .inflight
            .run(move || async move { check_once(&inner).await })
            .await;
        answer.unwrap_or(Err(UpdateError::Interrupted))
    }

    /// The status as of the last check.
    pub fn status(&self) -> UpdateStatus {
        self.inner.lock().clone()
    }
}

// One check from fetch to log line; `check` runs it at most once at a time.
async fn check_once(inner: &Inner) -> Result<UpdateStatus, UpdateError> {
    let result = fetch_and_apply(inner).await;
    let current = &inner.current;
    match &result {
        Ok((status, _, true)) => {
            if let Some(new) = status.latest() {
                tracing::info!(
                    "potluck {} is available (running {current}): {}; run `potluck admin update check` for the update steps",
                    new.version(),
                    new.notes_url()
                );
            }
        }
        Ok((_, version, false)) => {
            tracing::debug!("update check: newest signed release is {version}; running {current}");
        }
        Err(err @ UpdateError::NotFound) => {
            tracing::debug!("update check found nothing: {err}");
        }
        Err(err) => tracing::warn!(
            "update check failed: {err}; turn the daily check off with --no-update-check or POTLUCK_UPDATE_CHECK=false"
        ),
    }
    result.map(|(status, _, _)| status)
}

// Fetches, verifies and applies one answer, returning the new status, the verified version and
// whether `latest` moved; the caller logs.
async fn fetch_and_apply(inner: &Inner) -> Result<(UpdateStatus, Version, bool), UpdateError> {
    let client = build_client(&inner.source, &inner.current.to_string())?;
    let manifest = fetch(&client, &inner.source.base, MANIFEST_FILE).await?;
    let signature = fetch(&client, &inner.source.base, SIGNATURE_FILE).await?;
    let release = verify_release(&inner.source.keys, &manifest, &signature)?;
    let version = Version::parse(release.version())
        .map_err(|_| UpdateError::Manifest { field: "version" })?;
    // Under one guard, taken after the last await so the future stays Send: a final release
    // replaces `latest` only when it beats both the running version and the stored one, so a
    // replayed older manifest never pulls `latest` back.
    let mut state = inner.lock();
    let above_stored = state.latest.as_ref().is_none_or(|stored| {
        Version::parse(stored.version()).is_ok_and(|stored| version.cmp_precedence(&stored).is_gt())
    });
    let moved =
        version.pre.is_empty() && version.cmp_precedence(&inner.current).is_gt() && above_stored;
    if moved {
        state.latest = Some(release);
    }
    state.checked_at = Some(clock::now_rfc3339());
    Ok((state.clone(), version, moved))
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, UpdateStatus> {
        lock(&self.state)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

type Slot<T> = Arc<Mutex<Option<watch::Receiver<Option<T>>>>>;

/// At most one piece of work in flight; callers that arrive meanwhile share its answer.
pub(crate) struct SingleFlight<T> {
    slot: Slot<T>,
}

/// Empties the slot when the work ends, by return or by panic, so the next caller starts afresh.
struct ClearOnDrop<T>(Slot<T>);

impl<T> Drop for ClearOnDrop<T> {
    fn drop(&mut self) {
        *lock(&self.0) = None;
    }
}

impl<T: Clone + Send + Sync + 'static> SingleFlight<T> {
    pub(crate) fn new() -> Self {
        SingleFlight {
            slot: Arc::new(Mutex::new(None)),
        }
    }

    /// Joins the work in flight, or spawns `work()` in its own task so a caller that gives up
    /// cannot cancel it for the others. `None` when the work panicked.
    pub(crate) async fn run<F, Fut>(&self, work: F) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        // The lock only decides join or start; the work is built and spawned after it is
        // released, so a guard dropped by a panic or a refused spawn can clear the slot.
        let (mut rx, start) = {
            let mut slot = lock(&self.slot);
            match &*slot {
                Some(rx) => (rx.clone(), None),
                None => {
                    let (tx, rx) = watch::channel(None);
                    *slot = Some(rx.clone());
                    (rx, Some(tx))
                }
            }
        };
        if let Some(tx) = start {
            let guard = ClearOnDrop(Arc::clone(&self.slot));
            let fut = work();
            tokio::spawn(async move {
                let answer = fut.await;
                // Clear before publishing: a caller after the answer starts fresh work.
                drop(guard);
                let _ = tx.send(Some(answer));
            });
        }
        rx.wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|seen| seen.clone())
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
    use std::time::Duration;

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

    #[tokio::test]
    async fn a_panicking_check_frees_the_slot_for_the_next_one() {
        let flight = super::SingleFlight::<u32>::new();
        let first = flight
            .run(|| async { panic!("the check task panics") })
            .await;
        assert_eq!(first, None, "the waiter sees the interruption");
        let second = flight.run(|| async { 7 }).await;
        assert_eq!(second, Some(7), "the slot was cleared, so fresh work runs");
    }

    // Multi-threaded so the timeouts can fire even if a worker thread is stuck on the slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_work_closure_frees_the_slot_without_hanging() {
        let flight = std::sync::Arc::new(super::SingleFlight::<u32>::new());
        let starter = std::sync::Arc::clone(&flight);
        let first = tokio::spawn(async move {
            starter
                .run(|| -> std::future::Ready<u32> { panic!("building the work panics") })
                .await
        });
        let first = tokio::time::timeout(Duration::from_secs(3), first)
            .await
            .expect("the panicking caller does not hang");
        assert!(first.is_err(), "the caller sees the panic");
        let second = tokio::time::timeout(Duration::from_secs(3), flight.run(|| async { 7 }))
            .await
            .expect("the next caller does not hang");
        assert_eq!(second, Some(7), "the slot was cleared, so fresh work runs");
    }
}
