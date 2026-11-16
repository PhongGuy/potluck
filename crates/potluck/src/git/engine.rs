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
use crate::git::push::{CommandError, RefCommand, RefMove, list_refs, moved, parse_commands};
use crate::git::{Repos, Service, spool, sweep};

/// How many git slots exist, how long a request waits for one, how long an upload may hold one
/// before git starts, and how long each git child may run from spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct GitLimits {
    processes: usize,
    queue_wait: Duration,
    upload_timeout: Duration,
    max_runtime: Duration,
}

impl GitLimits {
    /// `processes` below 1 is raised to 1; each git child may run 30 min from spawn.
    pub fn new(processes: usize, queue_wait: Duration, upload_timeout: Duration) -> Self {
        Self {
            processes: processes.max(1),
            queue_wait,
            upload_timeout,
            max_runtime: Duration::from_secs(1800),
        }
    }

    /// The same limits with each git child allowed `max_runtime` from spawn; default 30 min.
    #[must_use]
    pub fn with_max_runtime(self, max_runtime: Duration) -> Self {
        Self {
            max_runtime,
            ..self
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

/// Told the moves of one receive-pack, once, after git exits with any status: the command list
/// against the refs before git started and as they stand after it exited (`moved`), an empty
/// list when nothing moved. Not called only when the refs cannot be read (logged at error).
pub(crate) type PushObserver = Box<dyn FnOnce(Vec<RefMove>) + Send + 'static>;

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
    max_runtime: Duration,
    writers: sweep::Writers,
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
            max_runtime: limits.max_runtime,
            writers: sweep::Writers::default(),
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
    /// `UploadPack`, followed by git's own advertisement. A git still running at `max_runtime`
    /// has its process group killed and is `Overran`.
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
        let mut child = self
            .command(service, v2, &[])
            .arg("--http-backend-info-refs")
            .arg(repo)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(GitError::Spawn)?;
        let (mut stdout, mut stderr) = match (child.stdout.take(), child.stderr.take()) {
            (Some(out), Some(err)) => (out, err),
            _ => return Err(GitError::Spawn(std::io::Error::other("git has no pipes"))),
        };
        // Drain both pipes and wait inside the run limit; the child stays owned here, so on
        // expiry its group is killed while the leader is still unreaped.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let bounded = tokio::time::timeout(self.max_runtime, async {
            tokio::join!(
                stdout.read_to_end(&mut out),
                stderr.read_to_end(&mut err),
                child.wait()
            )
        })
        .await;
        let status = match bounded {
            Ok((read_out, read_err, status)) => {
                read_out.map_err(GitError::Spawn)?;
                read_err.map_err(GitError::Spawn)?;
                status.map_err(GitError::Spawn)?
            }
            Err(_) => {
                kill_group(&child);
                let _ = child.wait().await;
                tracing::warn!(
                    service = service.name(),
                    limit = ?self.max_runtime,
                    "git ran past its run limit; killed its process group"
                );
                return Err(GitError::Overran {
                    service: service.name(),
                    limit: self.max_runtime,
                });
            }
        };
        if !status.success() {
            return Err(GitError::Failed {
                stderr: String::from_utf8_lossy(&err).trim().to_owned(),
            });
        }
        // A v2 upload-pack client expects the capability list first, as git-http-backend sends
        // it; every other combination opens with the service pkt-line and a flush.
        let mut body = Vec::with_capacity(out.len() + 40);
        if !(v2 && service == Service::UploadPack) {
            let name = service.name();
            body.extend_from_slice(
                format!("{:04x}# service={name}\n0000", 4 + 10 + name.len() + 1).as_bytes(),
            );
        }
        body.extend_from_slice(&out);
        Ok(body)
    }

    /// Serves one `POST git-upload-pack` or `git-receive-pack`: takes a slot, spools the body
    /// within `upload_timeout`, spawns git with the spool as stdin and `extra_env` in its
    /// environment, and returns its stdout as the response body. The slot is released when git
    /// exits. A receive-pack over `MAX_COMMANDS` commands is `TooManyCommands` before git starts;
    /// `on_push`, when given, hears the moves the push achieved once git has exited.
    // Each argument is one facet of the request the route already holds; bundling them would
    // only rename the list.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn rpc(
        &self,
        service: Service,
        repo: &Path,
        v2: bool,
        encoding: Option<&str>,
        extra_env: &[(String, String)],
        body: Body,
        on_push: Option<PushObserver>,
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
        let (stdin, commands) = if service == Service::ReceivePack {
            read_commands(stdin).await?
        } else {
            (stdin, Vec::new())
        };
        // A receive-pack holds the repository's writer gate until it is reaped, so a sweep after
        // a killed push never overlaps a live one; a `?` below drops it and releases the gate.
        let writer = if service == Service::ReceivePack {
            Some(self.writers.enter(repo).await)
        } else {
            None
        };
        // The refs before git starts are the baseline the reaper diffs against; nobody listening
        // means no extra git process.
        let watch = match on_push {
            Some(on_push) => Some((on_push, list_refs(self.repos.git(), repo).await?)),
            None => None,
        };
        let git = self.repos.git().to_path_buf();
        let repo_path = repo.to_path_buf();
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
        let max_runtime = self.max_runtime;
        // The reaper owns the child and the slot: the slot returns when git exits or is killed at
        // the run limit, and a non-zero exit is logged with the tail of stderr. The drain sits
        // inside the limit too, since the hook inherits stderr and could otherwise hold it open.
        tokio::spawn(async move {
            let _permit = permit;
            let bounded = tokio::time::timeout(max_runtime, async {
                tokio::join!(stderr_tail(stderr), child.wait())
            })
            .await;
            let killed = match bounded {
                Ok((_, Ok(status))) if status.success() => false,
                Ok((tail, Ok(status))) => {
                    tracing::warn!(
                        service = service.name(),
                        %status,
                        stderr = %tail,
                        "git exited with an error"
                    );
                    false
                }
                Ok((_, Err(err))) => {
                    tracing::warn!(service = service.name(), error = %err, "waiting for git failed");
                    false
                }
                Err(_) => {
                    // Kill the whole group while the leader is unreaped, so its pid cannot have
                    // been reused, then reap it. The cut-off stderr tail is not worth logging.
                    kill_group(&child);
                    let _ = child.wait().await;
                    tracing::warn!(
                        service = service.name(),
                        limit = ?max_runtime,
                        "git ran past its run limit; killed its process group"
                    );
                    true
                }
            };
            // Sweep before the observer hears the moves, so a listener sees a clean repository.
            if let Some(writer) = writer {
                writer.leave(killed).await;
            }
            // Whatever the exit status, the refs are the ground truth: a client that hangs up
            // mid-report kills git with SIGPIPE after the refs already moved.
            if let Some((on_push, before)) = watch {
                match list_refs(&git, &repo_path).await {
                    Ok(after) => on_push(moved(&commands, &before, &after)),
                    Err(err) => tracing::error!(error = %err, "cannot list refs after a push"),
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
        // A fresh process group that the hook and its children inherit, so the run limit can
        // kill them all at once.
        cmd.process_group(0);
        cmd.kill_on_drop(true);
        cmd
    }
}

/// SIGKILL to every process in `child`'s group, which `child` leads (spawned with
/// `process_group(0)`). A no-op once tokio has reaped the child; ESRCH is ignored.
fn kill_group(child: &tokio::process::Child) {
    use rustix::process::{Pid, Signal, kill_process_group};
    let Some(pid) = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
    else {
        return;
    };
    match kill_process_group(pid, Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
        Err(err) => tracing::warn!(error = %err, "cannot kill the git process group"),
    }
}

/// Parses the receive-pack command list off the spool and hands the file back rewound. Over the
/// cap is `TooManyCommands`; any other parse failure is logged and counts as no commands.
async fn read_commands(
    mut file: std::fs::File,
) -> Result<(std::fs::File, Vec<RefCommand>), GitError> {
    use std::io::Seek;
    let (file, parsed) = tokio::task::spawn_blocking(move || {
        let parsed = parse_commands(&mut file);
        file.rewind().map(|()| (file, parsed))
    })
    .await
    .map_err(|err| GitError::Spawn(std::io::Error::other(err)))?
    .map_err(GitError::Spawn)?;
    match parsed {
        Ok(commands) => Ok((file, commands)),
        Err(CommandError::TooMany { limit }) => Err(GitError::TooManyCommands { limit }),
        Err(err) => {
            tracing::warn!(error = %err, "cannot parse the receive-pack command list");
            Ok((file, Vec::new()))
        }
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
    /// More than `limit` commands in one receive-pack request: 400 `invalid_request`.
    #[error("receive-pack request carries more than {limit} commands")]
    TooManyCommands {
        /// The cap, `MAX_COMMANDS`.
        limit: usize,
    },
    /// `<hooks>/pre-receive` is not a regular executable file, so git would accept every push
    /// unchecked: 500 `internal`, logged.
    #[error("pre-receive hook missing or not executable at {}", path.display())]
    HookMissing {
        /// The hook path.
        path: PathBuf,
    },
    /// git ran past `max_runtime` and its process group was killed: 500 `internal`, logged.
    #[error("git {service} ran past its {limit:?} run limit and was killed")]
    Overran {
        /// `Service::name()` of the killed child.
        service: &'static str,
        /// The limit it ran past.
        limit: Duration,
    },
}

impl From<GitError> for ApiError {
    // Client faults carry their own text; server faults reach the operator's log only, since
    // git's stderr and io errors can name paths on the server.
    fn from(err: GitError) -> Self {
        match err {
            GitError::Busy => ApiError::unavailable("git is busy; retry shortly"),
            GitError::TooLarge { .. } => ApiError::payload_too_large(err.to_string()),
            GitError::BadEncoding(_)
            | GitError::SlowBody(_)
            | GitError::Body(_)
            | GitError::TooManyCommands { .. } => ApiError::invalid_request(err.to_string()),
            GitError::Spawn(_)
            | GitError::Failed { .. }
            | GitError::HookMissing { .. }
            | GitError::Overran { .. } => {
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
                None,
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
                .rpc(Service::UploadPack, &repo, true, None, &[], body, None)
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
            .rpc(Service::ReceivePack, &repo, false, None, &[], stalled, None)
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
                .rpc(
                    Service::UploadPack,
                    &repo,
                    true,
                    Some("gzip"),
                    &[],
                    body,
                    None,
                )
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
                None,
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

    /// `text` at `path`, mode 0755: a hook other than the `pre-receive` `write_stub_hook` writes.
    fn write_hook_file(path: &Path, text: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A receive-pack body creating `refs/heads/main` at the tip of a fresh work tree under
    /// `scratch`: the command pkt-line with `report-status`, a flush, then the pack holding every
    /// object the tip reaches. Returns the body and the tip.
    fn push_body(scratch: &Path) -> (Vec<u8>, String) {
        push_body_for(scratch, "main")
    }

    /// As `push_body`, for `refs/heads/<branch>` from a work tree of its own with distinct content.
    fn push_body_for(scratch: &Path, branch: &str) -> (Vec<u8>, String) {
        let run = |args: &[&str], cwd: &Path, stdin: &[u8]| -> Vec<u8> {
            use std::io::Write;
            let mut child = std::process::Command::new("git")
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
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(stdin).unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            out.stdout
        };
        let work = scratch.join(format!("push-{branch}"));
        std::fs::create_dir_all(&work).unwrap();
        run(&["init", "-q", "-b", "main"], &work, b"");
        std::fs::write(work.join("README"), format!("{branch}\n")).unwrap();
        run(&["add", "README"], &work, b"");
        run(&["commit", "-q", "-m", "push"], &work, b"");
        let tip = String::from_utf8(run(&["rev-parse", "HEAD"], &work, b""))
            .unwrap()
            .trim()
            .to_owned();
        let pack = run(
            &["pack-objects", "--stdout", "--revs", "-q"],
            &work,
            format!("{tip}\n").as_bytes(),
        );
        let line = format!(
            "{} {tip} refs/heads/{branch}\0report-status\n",
            crate::git::push::ZERO_SHA
        );
        let mut body = format!("{:04x}", line.len() + 4).into_bytes();
        body.extend_from_slice(line.as_bytes());
        body.extend_from_slice(b"0000");
        body.extend_from_slice(&pack);
        (body, tip)
    }

    /// An observer and the receiver its moves arrive on.
    fn observer() -> (
        PushObserver,
        tokio::sync::oneshot::Receiver<Vec<crate::git::push::RefMove>>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (
            Box::new(move |moves| {
                let _ = tx.send(moves);
            }),
            rx,
        )
    }

    #[tokio::test]
    async fn the_observer_hears_the_moves_even_when_git_dies_after_moving_them() {
        use crate::git::push::{RefMove, ZERO_SHA, list_refs};
        for kill in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (engine, repo) = engine_in(dir.path(), GitLimits::default()).await;
            if kill {
                // post-receive runs after the refs are updated; git runs builtins in-process and
                // executes the hook file directly, so `$PPID` is receive-pack. Killing it is the
                // non-zero exit with the refs already moved that a client hanging up mid-report
                // causes through SIGPIPE.
                write_hook_file(
                    &dir.path().join("hooks").join("post-receive"),
                    "#!/bin/sh\nkill -9 $PPID\n",
                );
            }
            let (body, tip) = push_body(dir.path());
            let (on_push, moved) = observer();
            let out = engine
                .rpc(
                    Service::ReceivePack,
                    &repo,
                    false,
                    None,
                    &[],
                    Body::from(body),
                    Some(on_push),
                )
                .await
                .unwrap();
            let _ = collect(out).await;
            let moves = tokio::time::timeout(Duration::from_secs(10), moved)
                .await
                .expect("the observer is called")
                .expect("not dropped");
            assert_eq!(
                moves,
                vec![RefMove::new("refs/heads/main", ZERO_SHA, &tip)],
                "kill={kill}"
            );
            let refs = list_refs(engine.repos.git(), &repo).await.unwrap();
            assert_eq!(
                refs.get("refs/heads/main"),
                Some(&tip),
                "kill={kill}: the repository holds the tip"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_list_reports_no_moves_and_a_list_over_the_cap_is_refused() {
        use crate::git::push::{MAX_COMMANDS, ZERO_SHA};
        let dir = tempfile::tempdir().unwrap();
        let (engine, repo) = engine_in(dir.path(), GitLimits::default()).await;
        let (on_push, moved) = observer();
        let out = engine
            .rpc(
                Service::ReceivePack,
                &repo,
                false,
                None,
                &[],
                Body::from("0000"),
                Some(on_push),
            )
            .await
            .unwrap();
        let _ = collect(out).await;
        let moves = tokio::time::timeout(Duration::from_secs(10), moved)
            .await
            .unwrap()
            .unwrap();
        assert!(moves.is_empty(), "told, with nothing: {moves:?}");
        let mut body = Vec::new();
        for i in 0..=MAX_COMMANDS {
            let line = format!("{ZERO_SHA} {} refs/heads/b{i}\n", "a".repeat(40));
            body.extend_from_slice(format!("{:04x}", line.len() + 4).as_bytes());
            body.extend_from_slice(line.as_bytes());
        }
        body.extend_from_slice(b"0000");
        let (on_push, moved) = observer();
        let err = engine
            .rpc(
                Service::ReceivePack,
                &repo,
                false,
                None,
                &[],
                Body::from(body),
                Some(on_push),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, GitError::TooManyCommands { limit } if limit == MAX_COMMANDS),
            "got {err:?}"
        );
        assert!(moved.await.is_err(), "the observer was dropped uncalled");
        let api = ApiError::from(GitError::TooManyCommands { limit: 1 });
        assert_eq!(
            (api.status(), api.code().as_str()),
            (400, "invalid_request")
        );
        assert_eq!(
            engine.permits.available_permits(),
            GitLimits::default().processes,
            "the slot came back"
        );
    }

    /// Whether `pid` has exited: gone from /proc, or a zombie waiting for its new parent.
    fn is_dead(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with('Z')),
        }
    }

    /// The pid a test script wrote to `path`; panics when the script never got that far.
    fn read_pid(path: &Path) -> u32 {
        std::fs::read_to_string(path)
            .expect("the script wrote its pid before the limit; raise the limit if not")
            .trim()
            .parse()
            .expect("a pid")
    }

    /// Polls `cond` every 20 ms for up to `secs`; true once it holds.
    async fn eventually(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cond()
    }

    /// The debris a killed receive-pack can leave in `repo`, as repo-relative paths.
    fn debris(repo: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut stack = vec![repo.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                let keep = name.ends_with(".keep")
                    && std::fs::read_to_string(&path)
                        .is_ok_and(|text| text.starts_with("receive-pack "));
                if name.starts_with("tmp_objdir-incoming-") || name.ends_with(".lock") || keep {
                    found.push(path.strip_prefix(repo).unwrap().display().to_string());
                } else if path.is_dir() {
                    stack.push(path);
                }
            }
        }
        found
    }

    /// The first line a hook wrote to `path`, once it is complete.
    async fn line_from(path: &Path) -> String {
        assert!(
            eventually(20, || std::fs::read_to_string(path)
                .is_ok_and(|t| t.ends_with('\n')))
            .await,
            "the hook wrote {}",
            path.display()
        );
        std::fs::read_to_string(path).unwrap().trim().to_owned()
    }

    /// One receive-pack of `body` with an observer; the moves it heard.
    async fn push_once(
        engine: &GitEngine,
        repo: &Path,
        body: Vec<u8>,
    ) -> Vec<crate::git::push::RefMove> {
        let (on_push, moved) = observer();
        let out = engine
            .rpc(
                Service::ReceivePack,
                repo,
                false,
                None,
                &[],
                Body::from(body),
                Some(on_push),
            )
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(30), collect(out)).await;
        tokio::time::timeout(Duration::from_secs(30), moved)
            .await
            .expect("the observer is called")
            .expect("not dropped")
    }

    #[tokio::test]
    async fn a_push_killed_in_pre_receive_leaves_no_quarantine_and_the_retry_lands() {
        use crate::git::push::{RefMove, ZERO_SHA};
        let dir = tempfile::tempdir().unwrap();
        let short = limits(1, 100).with_max_runtime(Duration::from_millis(1500));
        let (engine, repo) = engine_in(dir.path(), short).await;
        let seen = dir.path().join("quarantine.path");
        let hook = dir.path().join("hooks").join("pre-receive");
        write_hook_file(
            &hook,
            &format!(
                "#!/bin/sh\necho \"$GIT_QUARANTINE_PATH\" > '{}'\nexec sleep 30\n",
                seen.display()
            ),
        );
        let (body, tip) = push_body(dir.path());
        assert!(
            push_once(&engine, &repo, body.clone()).await.is_empty(),
            "killed"
        );
        let quarantine = line_from(&seen).await;
        assert!(quarantine.contains("tmp_objdir-incoming-"), "{quarantine}");
        assert!(!Path::new(&quarantine).exists(), "the quarantine is gone");
        assert_eq!(debris(&repo), Vec::<String>::new());
        write_hook_file(&hook, "#!/bin/sh\nexit 0\n");
        assert_eq!(
            push_once(&engine, &repo, body).await,
            vec![RefMove::new("refs/heads/main", ZERO_SHA, &tip)]
        );
    }

    #[test]
    fn the_default_run_limit_is_thirty_minutes() {
        assert_eq!(GitLimits::default().max_runtime, Duration::from_secs(1800));
        assert_eq!(limits(1, 100).max_runtime, Duration::from_secs(1800));
        let short = limits(1, 100).with_max_runtime(Duration::from_millis(5));
        assert_eq!(
            short.max_runtime,
            Duration::from_millis(5),
            "builder sets it"
        );
    }

    #[tokio::test]
    async fn rpc_kills_a_hook_past_the_run_limit_and_frees_the_slot() {
        use crate::git::push::list_refs;
        let dir = tempfile::tempdir().unwrap();
        let short = limits(1, 100).with_max_runtime(Duration::from_millis(1500));
        let (engine, repo) = engine_in(dir.path(), short).await;
        let pidfile = dir.path().join("hook.pid");
        write_hook_file(
            &dir.path().join("hooks").join("pre-receive"),
            &format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
                pidfile.display()
            ),
        );
        let (body, _tip) = push_body(dir.path());
        let (on_push, moved) = observer();
        let started = std::time::Instant::now();
        let out = engine
            .rpc(
                Service::ReceivePack,
                &repo,
                false,
                None,
                &[],
                Body::from(body),
                Some(on_push),
            )
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(10), collect(out))
            .await
            .expect("the body ends once the group is killed");
        assert!(
            eventually(10, || engine.permits.available_permits() == 1).await,
            "the slot came back"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "killed near the limit, not after the hook's 30 s: {:?}",
            started.elapsed()
        );
        let pid = read_pid(&pidfile);
        assert!(
            eventually(5, || is_dead(pid)).await,
            "the hook {pid} was killed too"
        );
        let moves = tokio::time::timeout(Duration::from_secs(10), moved)
            .await
            .expect("the observer is called after a kill")
            .expect("not dropped");
        assert!(moves.is_empty(), "the hook never accepted: {moves:?}");
        let refs = list_refs(engine.repos.git(), &repo).await.unwrap();
        assert!(!refs.contains_key("refs/heads/main"), "nothing was pushed");
    }

    #[tokio::test]
    async fn a_hook_inside_the_run_limit_is_not_killed() {
        let dir = tempfile::tempdir().unwrap();
        let roomy = limits(1, 100).with_max_runtime(Duration::from_secs(10));
        let (engine, repo) = engine_in(dir.path(), roomy).await;
        write_hook_file(
            &dir.path().join("hooks").join("pre-receive"),
            "#!/bin/sh\nsleep 1\nexit 0\n",
        );
        let (body, tip) = push_body(dir.path());
        let (on_push, moved) = observer();
        let out = engine
            .rpc(
                Service::ReceivePack,
                &repo,
                false,
                None,
                &[],
                Body::from(body),
                Some(on_push),
            )
            .await
            .unwrap();
        let _ = collect(out).await;
        let moves = tokio::time::timeout(Duration::from_secs(10), moved)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(moves.len(), 1, "the push landed: {moves:?}");
        assert_eq!(moves[0].new_sha(), tip, "at the pushed tip");
    }

    #[tokio::test]
    async fn a_hung_advertisement_is_killed_with_its_children_and_answers_500() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // A stand-in git that never answers and leaves a child of its own behind.
        let pidfile = dir.path().join("child.pid");
        let fake = dir.path().join("fake-git");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nwait\n",
                pidfile.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(dir.path().join("tmp")).unwrap();
        write_stub_hook(&dir.path().join("hooks"), 0o755);
        let engine = GitEngine::new(
            Repos::new(dir.path().join("repos"), fake),
            dir.path().join("hooks"),
            dir.path().join("tmp"),
            1024,
            limits(1, 100).with_max_runtime(Duration::from_millis(1500)),
        );
        let started = std::time::Instant::now();
        let err = engine
            .advertise(
                Service::UploadPack,
                &dir.path().join("repos").join("x.git"),
                false,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, GitError::Overran { service: "git-upload-pack", limit } if limit == Duration::from_millis(1500)),
            "got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(engine.permits.available_permits(), 1, "the slot is back");
        let pid = read_pid(&pidfile);
        assert!(
            eventually(5, || is_dead(pid)).await,
            "the grandchild {pid} was killed too"
        );
        let api = ApiError::from(err);
        assert_eq!((api.status(), api.code().as_str()), (500, "internal"));
    }
}
