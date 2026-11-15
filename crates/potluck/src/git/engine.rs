//! [`GitEngine`]: spawns `git upload-pack` and `git receive-pack` for the smart HTTP routes.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::api::error::ApiError;
use crate::git::{Repos, Service};

/// How many git slots exist, how long a request waits for one, and how long an upload may
/// hold one before git starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct GitLimits {
    processes: usize,
    queue_wait: Duration,
    upload_timeout: Duration,
}

impl GitLimits {
    /// `processes` below 1 is raised to 1.
    pub fn new(processes: usize, queue_wait: Duration, upload_timeout: Duration) -> Self {
        Self {
            processes: processes.max(1),
            queue_wait,
            upload_timeout,
        }
    }
}

impl Default for GitLimits {
    /// Two slots per core (at least two), a 30 s queue wait and a 10 min upload window.
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        Self::new(
            (2 * cores).max(2),
            Duration::from_secs(30),
            Duration::from_secs(600),
        )
    }
}

/// Spawns git for the smart HTTP routes within the configured limits.
#[derive(Debug)]
#[non_exhaustive]
pub struct GitEngine {
    repos: Repos,
    hooks_dir: PathBuf,
    #[expect(
        dead_code,
        reason = "the POST body spool reads it once the rpc path lands"
    )]
    tmp_dir: PathBuf,
    max_body: u64,
    permits: Arc<Semaphore>,
    queue_wait: Duration,
    #[expect(
        dead_code,
        reason = "the POST body spool reads it once the rpc path lands"
    )]
    upload_timeout: Duration,
}

impl GitEngine {
    /// `hooks_dir` is `Config::hooks_dir()`, `tmp_dir` is `Config::tmp_dir()` and `max_body` is
    /// `Config::max_push_bytes()`. Both directories are absolute, since `Config` made `data`
    /// absolute; the engine does not re-check.
    pub fn new(
        repos: Repos,
        hooks_dir: PathBuf,
        tmp_dir: PathBuf,
        max_body: u64,
        limits: GitLimits,
    ) -> Self {
        Self {
            repos,
            hooks_dir,
            tmp_dir,
            max_body,
            permits: Arc::new(Semaphore::new(limits.processes)),
            queue_wait: limits.queue_wait,
            upload_timeout: limits.upload_timeout,
        }
    }

    /// The repository layout this engine serves.
    pub fn repos(&self) -> &Repos {
        &self.repos
    }

    /// The body cap in bytes.
    pub fn max_body(&self) -> u64 {
        self.max_body
    }

    /// The `GET info/refs` body: the `# service=` pkt-line and flush, unless `v2` and
    /// `UploadPack`, followed by git's own advertisement.
    pub async fn advertise(
        &self,
        service: Service,
        repo: &Path,
        v2: bool,
    ) -> Result<Vec<u8>, GitError> {
        let _permit = self.acquire().await?;
        let output = self
            .command(service, v2, &[])
            .arg("--http-backend-info-refs")
            .arg(repo)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(GitError::Spawn)?;
        if !output.status.success() {
            return Err(GitError::Failed {
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        // A v2 upload-pack client expects the capability list first, as git-http-backend sends
        // it; every other combination opens with the service pkt-line and a flush.
        let mut body = Vec::with_capacity(output.stdout.len() + 40);
        if !(v2 && service == Service::UploadPack) {
            let name = service.name();
            body.extend_from_slice(
                format!("{:04x}# service={name}\n0000", 4 + 10 + name.len() + 1).as_bytes(),
            );
        }
        body.extend_from_slice(&output.stdout);
        Ok(body)
    }

    /// A process slot, or `Busy` once `queue_wait` has passed without one.
    async fn acquire(&self) -> Result<OwnedSemaphorePermit, GitError> {
        tokio::time::timeout(self.queue_wait, Arc::clone(&self.permits).acquire_owned())
            .await
            .map_err(|_| GitError::Busy)?
            .map_err(|_| GitError::Busy)
    }

    /// git with the hardening flags and a clean environment; the caller appends the
    /// service-specific flags and the repository path.
    fn command(&self, service: Service, v2: bool, extra_env: &[(String, String)]) -> Command {
        let mut cmd = Command::new(self.repos.git());
        cmd.arg("-c")
            .arg(format!("core.hooksPath={}", self.hooks_dir.display()))
            .arg("-c")
            .arg("receive.fsckObjects=true")
            .arg(service.subcommand())
            .arg("--stateless-rpc");
        // The server's own git config must never shape what is served, so the child starts from an
        // empty environment plus PATH, and global and system config are switched off.
        cmd.env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        if v2 {
            cmd.env("GIT_PROTOCOL", "version=2");
        }
        // What the resolver wants the hook to see (pusher, kind, override id, from sub-plan 5 on).
        cmd.envs(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        cmd.kill_on_drop(true);
        cmd
    }
}

/// Why a git request could not be served; `From<GitError> for ApiError` maps each variant.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GitError {
    /// No process slot came free within `queue_wait`: 503 `unavailable`.
    #[error("git is busy; retry shortly")]
    Busy,
    /// The body is above the cap: 413 `payload_too_large`.
    #[error("request body exceeds the {limit} byte limit")]
    TooLarge {
        /// The cap in bytes.
        limit: u64,
    },
    /// A `Content-Encoding` other than gzip: 400 `invalid_request`.
    #[error("unsupported Content-Encoding {0:?}; only gzip is accepted")]
    BadEncoding(String),
    /// The body did not finish arriving within `upload_timeout`: 400 `invalid_request`.
    #[error("request body did not complete within {0:?}")]
    SlowBody(Duration),
    /// The body could not be read or inflated: 400 `invalid_request`.
    #[error("request body could not be read: {0}")]
    Body(#[source] std::io::Error),
    /// The spool file or the child could not be set up: 500 `internal`.
    #[error("cannot run git: {0}")]
    Spawn(#[source] std::io::Error),
    /// git exited non-zero before any output was sent (advertisement only): 500 `internal`.
    #[error("git failed: {stderr}")]
    Failed {
        /// git's stderr, trimmed.
        stderr: String,
    },
}

impl From<GitError> for ApiError {
    // Client faults carry their own text; server faults reach the operator's log only, since
    // git's stderr and io errors can name paths on the server.
    fn from(err: GitError) -> Self {
        match err {
            GitError::Busy => ApiError::unavailable("git is busy; retry shortly"),
            GitError::TooLarge { .. } => ApiError::payload_too_large(err.to_string()),
            GitError::BadEncoding(_) | GitError::SlowBody(_) | GitError::Body(_) => {
                ApiError::invalid_request(err.to_string())
            }
            GitError::Spawn(_) | GitError::Failed { .. } => {
                tracing::error!(error = %err, "git error");
                ApiError::internal("git failed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::*;
    use crate::slug::Slug;

    pub(super) fn limits(processes: usize, queue_wait_ms: u64) -> GitLimits {
        GitLimits::new(
            processes,
            Duration::from_millis(queue_wait_ms),
            Duration::from_secs(60),
        )
    }

    pub(super) async fn engine_in(dir: &Path, limits: GitLimits) -> (GitEngine, PathBuf) {
        let repos = Repos::new(dir.join("repos"), PathBuf::from("git"));
        let repo = repos
            .init(
                &Slug::parse("core").unwrap(),
                &Slug::parse("api").unwrap(),
                "main",
            )
            .await
            .unwrap();
        std::fs::create_dir_all(dir.join("tmp")).unwrap();
        (
            GitEngine::new(
                repos,
                dir.join("hooks"),
                dir.join("tmp"),
                1024 * 1024,
                limits,
            ),
            repo,
        )
    }

    fn has(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[tokio::test]
    async fn upload_pack_v0_starts_with_the_service_line() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, repo) = engine_in(dir.path(), GitLimits::default()).await;
        let out = engine
            .advertise(Service::UploadPack, &repo, false)
            .await
            .unwrap();
        assert!(
            out.starts_with(b"001e# service=git-upload-pack\n0000"),
            "{}",
            String::from_utf8_lossy(&out)
        );
        assert!(
            has(&out, b"capabilities^{}"),
            "an empty repo advertises capabilities only"
        );
    }

    #[tokio::test]
    async fn upload_pack_v2_starts_with_version_2_and_no_service_line() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, repo) = engine_in(dir.path(), GitLimits::default()).await;
        let out = engine
            .advertise(Service::UploadPack, &repo, true)
            .await
            .unwrap();
        assert!(
            out.starts_with(b"000eversion 2\n"),
            "{}",
            String::from_utf8_lossy(&out)
        );
        assert!(!has(&out, b"# service"), "v2 has no service line");
        assert!(has(&out, b"ls-refs"), "v2 lists its commands");
    }

    #[tokio::test]
    async fn receive_pack_always_has_the_service_line() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, repo) = engine_in(dir.path(), GitLimits::default()).await;
        for v2 in [false, true] {
            let out = engine
                .advertise(Service::ReceivePack, &repo, v2)
                .await
                .unwrap();
            assert!(
                out.starts_with(b"001f# service=git-receive-pack\n0000"),
                "v2={v2}: {}",
                String::from_utf8_lossy(&out)
            );
            assert!(
                has(&out, b"report-status"),
                "v2={v2}: receive-pack capabilities"
            );
        }
    }

    #[tokio::test]
    async fn a_missing_repository_is_a_failure_with_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _repo) = engine_in(dir.path(), GitLimits::default()).await;
        let err = engine
            .advertise(Service::UploadPack, &dir.path().join("nope.git"), false)
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::Failed { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn a_full_engine_answers_busy_after_the_queue_wait() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, repo) = engine_in(dir.path(), limits(1, 100)).await;
        let held = Arc::clone(&engine.permits).acquire_owned().await.unwrap();
        let err = engine
            .advertise(Service::UploadPack, &repo, false)
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::Busy), "got {err:?}");
        drop(held);
        engine
            .advertise(Service::UploadPack, &repo, false)
            .await
            .unwrap();
    }

    #[test]
    fn limits_default_and_clamp() {
        let d = GitLimits::default();
        assert!(d.processes >= 2, "at least two slots");
        assert_eq!(d.queue_wait, Duration::from_secs(30));
        assert_eq!(d.upload_timeout, Duration::from_secs(600));
        assert_eq!(limits(0, 1000).processes, 1, "zero is raised to one");
    }

    #[test]
    fn service_names_round_trip() {
        assert_eq!(
            Service::from_name("git-upload-pack"),
            Some(Service::UploadPack)
        );
        assert_eq!(
            Service::from_name("git-receive-pack"),
            Some(Service::ReceivePack)
        );
        assert_eq!(Service::from_name("upload-pack"), None);
        assert_eq!(Service::UploadPack.subcommand(), "upload-pack");
        assert_eq!(
            Service::ReceivePack.advertisement_type(),
            "application/x-git-receive-pack-advertisement"
        );
        assert_eq!(
            Service::UploadPack.result_type(),
            "application/x-git-upload-pack-result"
        );
    }
}
