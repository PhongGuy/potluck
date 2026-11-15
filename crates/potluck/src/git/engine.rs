//! [`GitEngine`]: spawns `git upload-pack` and `git receive-pack` for the smart HTTP routes.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use tokio::io::AsyncReadExt;
use tokio::process::{ChildStderr, Command};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::api::error::ApiError;
use crate::git::{Repos, Service, spool};

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
    tmp_dir: PathBuf,
    max_body: u64,
    permits: Arc<Semaphore>,
    queue_wait: Duration,
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
        if service == Service::ReceivePack {
            self.require_hook()?;
        }
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

    /// Serves one `POST git-upload-pack` or `git-receive-pack`: takes a slot, spools the body
    /// within `upload_timeout`, spawns git with the spool as stdin and `extra_env` in its
    /// environment, and returns its stdout as the response body. The slot is released when git
    /// exits.
    pub(crate) async fn rpc(
        &self,
        service: Service,
        repo: &Path,
        v2: bool,
        encoding: Option<&str>,
        extra_env: &[(String, String)],
        body: Body,
    ) -> Result<Body, GitError> {
        if service == Service::ReceivePack {
            self.require_hook()?;
        }
        // The slot covers the spool too, so the disk holds at most `processes` bodies at once; the
        // upload timeout keeps a trickling client from parking on a slot.
        let permit = self.acquire().await?;
        // Spool before spawning: a 413 can still be answered, and git reads its request from a
        // file, so nothing has to feed stdin while git is already writing progress to stdout.
        let stdin = tokio::time::timeout(
            self.upload_timeout,
            spool::spool(body, encoding, self.max_body, &self.tmp_dir),
        )
        .await
        .map_err(|_| GitError::SlowBody(self.upload_timeout))??;
        let mut child = self
            .command(service, v2, extra_env)
            .arg(repo)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(GitError::Spawn)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| GitError::Spawn(std::io::Error::other("git has no stdout")))?;
        let stderr = child.stderr.take();
        // The reaper owns the child and the slot: the slot returns when git exits, and a non-zero
        // exit is logged with the tail of stderr. Nothing from the request reaches the log.
        tokio::spawn(async move {
            let _permit = permit;
            let tail = stderr_tail(stderr).await;
            match child.wait().await {
                Ok(status) if status.success() => {}
                Ok(status) => tracing::warn!(
                    service = service.name(),
                    %status,
                    stderr = %tail,
                    "git exited with an error"
                ),
                Err(err) => {
                    tracing::warn!(service = service.name(), error = %err, "waiting for git failed");
                }
            }
        });
        Ok(Body::from_stream(tokio_util::io::ReaderStream::new(stdout)))
    }

    /// `Ok` when `<hooks_dir>/pre-receive` is a regular file with any execute bit. git accepts a
    /// push unchecked when the hook is absent, so receive-pack is refused before it takes a slot.
    fn require_hook(&self) -> Result<(), GitError> {
        use std::os::unix::fs::PermissionsExt;
        let path = self.hooks_dir.join("pre-receive");
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() && meta.permissions().mode() & 0o111 != 0 => Ok(()),
            _ => Err(GitError::HookMissing { path }),
        }
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
        // What the resolver wants the hook to see, such as the pusher, kind and override id.
        cmd.envs(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        cmd.kill_on_drop(true);
        cmd
    }
}

/// Drains git's stderr to EOF so git never blocks on a full pipe, keeping only the last 8 KiB
/// for the log.
async fn stderr_tail(stderr: Option<ChildStderr>) -> String {
    const KEEP: usize = 8 * 1024;
    let Some(mut stderr) = stderr else {
        return String::new();
    };
    let mut tail = Vec::with_capacity(KEEP);
    let mut chunk = [0u8; 4096];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                tail.extend_from_slice(&chunk[..n]);
                let excess = tail.len().saturating_sub(KEEP);
                tail.drain(..excess);
            }
        }
    }
    String::from_utf8_lossy(&tail).trim().to_owned()
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
    /// `<hooks>/pre-receive` is not a regular executable file, so git would accept every push
    /// unchecked: 500 `internal`, logged.
    #[error("pre-receive hook missing or not executable at {}", path.display())]
    HookMissing {
        /// The hook path.
        path: PathBuf,
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
            GitError::Spawn(_) | GitError::Failed { .. } | GitError::HookMissing { .. } => {
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
        write_stub_hook(&dir.join("hooks"), 0o755);
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

    /// A `pre-receive` that accepts everything, with `mode`, so receive-pack may run.
    fn write_stub_hook(hooks: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(hooks).unwrap();
        let hook = hooks.join("pre-receive");
        std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(mode)).unwrap();
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
    async fn receive_pack_is_refused_without_an_executable_hook() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, repo) = engine_in(dir.path(), GitLimits::default()).await;
        std::fs::remove_file(dir.path().join("hooks").join("pre-receive")).unwrap();
        let err = engine
            .advertise(Service::ReceivePack, &repo, false)
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::HookMissing { .. }), "got {err:?}");
        let err = engine
            .rpc(
                Service::ReceivePack,
                &repo,
                false,
                None,
                &[],
                Body::from("0000"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::HookMissing { .. }), "got {err:?}");
        engine
            .advertise(Service::UploadPack, &repo, false)
            .await
            .unwrap();
        write_stub_hook(&dir.path().join("hooks"), 0o644);
        let err = engine
            .advertise(Service::ReceivePack, &repo, false)
            .await
            .unwrap_err();
        assert!(
            matches!(err, GitError::HookMissing { .. }),
            "not executable: {err:?}"
        );
        write_stub_hook(&dir.path().join("hooks"), 0o755);
        engine
            .advertise(Service::ReceivePack, &repo, false)
            .await
            .unwrap();
        assert_eq!(
            engine.permits.available_permits(),
            GitLimits::default().processes,
            "no slot was taken for a refused push"
        );
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

    /// One commit on `main` in `bare`, made in a scratch work tree and fetched in.
    pub(super) fn seed_commit(bare: &Path, scratch: &Path) {
        let run = |args: &[&str], cwd: &Path| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", scratch)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let work = scratch.join("work");
        std::fs::create_dir_all(&work).unwrap();
        run(&["init", "-q", "-b", "main"], &work);
        std::fs::write(work.join("README"), "seed\n").unwrap();
        run(&["add", "README"], &work);
        run(&["commit", "-q", "-m", "seed"], &work);
        run(
            &[
                "--git-dir",
                bare.to_str().unwrap(),
                "fetch",
                "-q",
                work.to_str().unwrap(),
                "main:main",
            ],
            scratch,
        );
    }

    async fn collect(body: Body) -> Vec<u8> {
        axum::body::to_bytes(body, 1 << 20).await.unwrap().to_vec()
    }

    #[tokio::test]
    async fn rpc_answers_a_v2_ls_refs_request() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, repo) = engine_in(dir.path(), GitLimits::default()).await;
        seed_commit(&repo, dir.path());
        let body = Body::from("0014command=ls-refs\n0000");
        let out = collect(
            engine
                .rpc(Service::UploadPack, &repo, true, None, &[], body)
                .await
                .unwrap(),
        )
        .await;
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("refs/heads/main"), "{text}");
    }

    #[tokio::test]
    async fn rpc_cuts_a_stalled_upload_at_the_upload_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let short = GitLimits::new(1, Duration::from_millis(100), Duration::from_millis(200));
        let (engine, repo) = engine_in(dir.path(), short).await;
        // A body that never yields a byte: the spool cannot finish, so the timeout must fire and
        // the slot must come back.
        let stalled = Body::from_stream(futures_util::stream::pending::<
            Result<axum::body::Bytes, std::io::Error>,
        >());
        let err = engine
            .rpc(Service::ReceivePack, &repo, false, None, &[], stalled)
            .await
            .unwrap_err();
        assert!(
            matches!(err, GitError::SlowBody(d) if d == Duration::from_millis(200)),
            "got {err:?}"
        );
        assert_eq!(engine.permits.available_permits(), 1, "the slot is back");
    }

    #[tokio::test]
    async fn rpc_inflates_a_gzip_request() {
        use tokio::io::AsyncWriteExt;
        let dir = tempfile::tempdir().unwrap();
        let (engine, repo) = engine_in(dir.path(), GitLimits::default()).await;
        seed_commit(&repo, dir.path());
        let mut enc = async_compression::tokio::write::GzipEncoder::new(Vec::new());
        enc.write_all(b"0014command=ls-refs\n0000").await.unwrap();
        enc.shutdown().await.unwrap();
        let body = Body::from(enc.into_inner());
        let out = collect(
            engine
                .rpc(Service::UploadPack, &repo, true, Some("gzip"), &[], body)
                .await
                .unwrap(),
        )
        .await;
        assert!(String::from_utf8_lossy(&out).contains("refs/heads/main"));
    }

    #[tokio::test]
    async fn rpc_refuses_an_oversized_body_before_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let repos = Repos::new(dir.path().join("repos"), PathBuf::from("git"));
        let repo = repos
            .init(
                &Slug::parse("t").unwrap(),
                &Slug::parse("w").unwrap(),
                "main",
            )
            .await
            .unwrap();
        std::fs::create_dir_all(dir.path().join("tmp")).unwrap();
        write_stub_hook(&dir.path().join("hooks"), 0o755);
        let engine = GitEngine::new(
            repos,
            dir.path().join("hooks"),
            dir.path().join("tmp"),
            16,
            limits(1, 200),
        );
        let err = engine
            .rpc(
                Service::ReceivePack,
                &repo,
                false,
                None,
                &[],
                Body::from(vec![0u8; 17]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, GitError::TooLarge { limit: 16 }),
            "got {err:?}"
        );
        assert_eq!(
            engine.permits.available_permits(),
            1,
            "the refused request returned its slot"
        );
        engine
            .advertise(Service::UploadPack, &repo, false)
            .await
            .unwrap();
    }
}
