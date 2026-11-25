//! [`GitEngine`]: spawns `git upload-pack` and `git receive-pack` for the smart HTTP routes.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use tokio::io::AsyncReadExt;
use tokio::process::{ChildStderr, Command};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::task::TaskTracker;

use crate::api::error::ApiError;
use crate::git::push::{CommandError, RefCommand, RefMove, list_refs, moved, parse_commands};
use crate::git::report::{self, HookReport};
use crate::git::{Repos, Service, spool, sweep};
use crate::metrics::{GitOutcome, GitService, Metrics};

/// The default for how long a git child may run from spawn: 30 min.
pub(crate) const DEFAULT_MAX_RUNTIME_SECS: u64 = 1800;

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
            max_runtime: Duration::from_secs(DEFAULT_MAX_RUNTIME_SECS),
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
/// list when nothing moved, with the hook's refusal report (the default when there was no report
/// file). Not called only when the refs cannot be read (logged at error).
pub(crate) type PushObserver = Box<dyn FnOnce(Vec<RefMove>, HookReport) + Send + 'static>;

/// Spawns git for the smart HTTP routes within the configured limits.
#[derive(Debug)]
#[non_exhaustive]
pub struct GitEngine {
    repos: Repos,
    hooks_dir: PathBuf,
    tmp_dir: PathBuf,
    max_body: u64,
    permits: Arc<Semaphore>,
    processes: usize,
    queue_wait: Duration,
    upload_timeout: Duration,
    max_runtime: Duration,
    writers: sweep::Writers,
    tasks: TaskTracker,
    metrics: Arc<Metrics>,
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
            repos: repos.with_max_runtime(limits.max_runtime),
            hooks_dir,
            tmp_dir,
            max_body,
            permits: Arc::new(Semaphore::new(limits.processes)),
            processes: limits.processes,
            queue_wait: limits.queue_wait,
            upload_timeout: limits.upload_timeout,
            max_runtime: limits.max_runtime,
            writers: sweep::Writers::default(),
            tasks: TaskTracker::new(),
            metrics: Metrics::new(env!("CARGO_PKG_VERSION")),
        }
    }

    /// Records request outcomes and slot waits into `metrics` instead of a private registry.
    pub(crate) fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Git slots held now and the configured limit, as `(in_use, limit)`.
    pub(crate) fn slots(&self) -> (u64, u64) {
        let limit = self.processes as u64;
        let free = self.permits.available_permits() as u64;
        (limit.saturating_sub(free), limit)
    }

    /// Marks each of `repos` dirty, so the next push to leave it sweeps it.
    pub(crate) fn mark_dirty(&self, repos: &[PathBuf]) {
        self.writers.mark_dirty(repos);
    }

    /// The tracker every reaper, push record and workspace create or delete is spawned on, so
    /// shutdown can wait for them before the database closes.
    pub(crate) fn tasks(&self) -> &TaskTracker {
        &self.tasks
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
        let result = self.advertise_unrecorded(service, repo, v2).await;
        let outcome = match &result {
            Ok(_) => Some(GitOutcome::Ok),
            Err(GitError::Busy) => None,
            Err(GitError::Overran { .. }) => Some(GitOutcome::Overran),
            Err(_) => Some(GitOutcome::Failed),
        };
        // `acquire` already counted a `Busy`, so each advertisement is counted exactly once.
        if let Some(outcome) = outcome {
            self.metrics.git_request(GitService::Advertise, outcome);
        }
        result
    }

    async fn advertise_unrecorded(
        &self,
        service: Service,
        repo: &Path,
        v2: bool,
    ) -> Result<Vec<u8>, GitError> {
        if service == Service::ReceivePack {
            self.require_hook()?;
        }
        let _permit = self.acquire(GitService::Advertise).await?;
        let mut child = self
            .command(service, v2, &[])
            .arg("--http-backend-info-refs")
            .arg(repo)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(GitError::Spawn)?;
        let group = process_group(&child);
        let guard = GroupKill(group);
        let (mut stdout, mut stderr) = match (child.stdout.take(), child.stderr.take()) {
            (Some(out), Some(err)) => (out, err),
            _ => return Err(GitError::Spawn(std::io::Error::other("git has no pipes"))),
        };
        // Drain both pipes to EOF before reaping, both inside the run limit: an unreaped leader
        // keeps the group id from being reused, so the kill reaches only git's own group.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let bounded = tokio::time::timeout(self.max_runtime, async {
            let (read_out, read_err) =
                tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
            (read_out, read_err, child.wait().await)
        })
        .await;
        let status = match bounded {
            Ok((read_out, read_err, status)) => {
                if status.is_ok() {
                    guard.disarm();
                }
                read_out.map_err(GitError::Spawn)?;
                read_err.map_err(GitError::Spawn)?;
                status.map_err(GitError::Spawn)?
            }
            Err(_) => {
                let err = overran(&mut child, group, service, self.max_runtime).await;
                guard.disarm();
                return Err(err);
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
    /// and every process left in its group have exited, or at the run limit. A receive-pack over
    /// `MAX_COMMANDS` commands is `TooManyCommands` before git starts;
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
        let result = self
            .rpc_unrecorded(service, repo, v2, encoding, extra_env, body, on_push)
            .await;
        // A request that never reached the reaper is counted here; `acquire` counted a `Busy`,
        // and the reaper counts every request it took over.
        match &result {
            Ok(_) | Err(GitError::Busy) => {}
            Err(_) => self
                .metrics
                .git_request(git_service(service), GitOutcome::Failed),
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn rpc_unrecorded(
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
        // The slot comes before the writer gate, so a push refused `Busy` never held the gate and
        // cannot strand a killed push's deferred sweep.
        let permit = self.acquire(git_service(service)).await?;
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
        // a killed push never overlaps a live one. After `enter`, every exit goes through `leave`,
        // so a sweep an earlier killed push deferred runs even when this push fails before git.
        let writer = if service == Service::ReceivePack {
            Some(self.writers.enter(repo).await)
        } else {
            None
        };
        // The refs before git starts are the baseline the reaper diffs against; nobody listening
        // means no extra git process.
        let watch = match on_push {
            Some(on_push) => match list_refs(&self.repos, repo).await {
                Ok(refs) => Some((on_push, refs)),
                Err(err) => return Err(released(writer, err).await),
            },
            None => None,
        };
        let repos = self.repos.clone();
        let repo_path = repo.to_path_buf();
        let report_file = if service == Service::ReceivePack {
            self.report_file()
        } else {
            None
        };
        let mut cmd = self.command(service, v2, extra_env);
        // The engine is the sole author of the report path: with no file, no inherited or
        // resolver-supplied value may reach the hook.
        match &report_file {
            Some(file) => cmd.env(crate::secret_check::REPORT_ENV, file.path()),
            None => cmd.env_remove(crate::secret_check::REPORT_ENV),
        };
        let spawned = cmd
            .arg(repo)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(err) => return Err(released(writer, GitError::Spawn(err)).await),
        };
        let group = process_group(&child);
        debug_assert!(
            child.stdout.is_some(),
            "a piped stdout is present after spawn"
        );
        // Unreachable while stdout is piped; if it ever happens, git is killed like an overrun
        // and the repository is swept before the gate opens.
        let Some(stdout) = child.stdout.take() else {
            kill_and_reap(&mut child, group).await;
            if let Some(writer) = writer {
                writer.leave(true).await;
            }
            return Err(GitError::Spawn(std::io::Error::other("git has no stdout")));
        };
        let stderr = child.stderr.take();
        let max_runtime = self.max_runtime;
        let metrics = Arc::clone(&self.metrics);
        // The reaper owns the child and the slot: the slot returns when git's group is empty or is
        // killed at the run limit, and a non-zero exit is logged with the tail of stderr.
        self.tasks.spawn(async move {
            let _permit = permit;
            // Wait for git's whole group inside the run limit: stderr to EOF, the leader to exit
            // (left unreaped so the group id cannot be reused), then any process still in it.
            let pidfd = leader_pidfd(&child);
            let bounded = tokio::time::timeout(max_runtime, async {
                let tail = stderr_tail(stderr).await;
                if let (Some(pidfd), Some(group)) = (pidfd, group) {
                    exited(pidfd).await;
                    while strays_in_group(group).await {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
                (tail, child.wait().await)
            })
            .await;
            let outcome = match bounded {
                Ok((_, Ok(status))) if status.success() => GitOutcome::Ok,
                Ok((tail, Ok(status))) => {
                    tracing::warn!(
                        service = service.name(),
                        %status,
                        stderr = %tail,
                        "git exited with an error"
                    );
                    GitOutcome::Failed
                }
                Ok((_, Err(err))) => {
                    tracing::warn!(service = service.name(), error = %err, "waiting for git failed");
                    GitOutcome::Failed
                }
                Err(_) => {
                    // The cut-off stderr tail is not worth logging.
                    let _ = overran(&mut child, group, service, max_runtime).await;
                    GitOutcome::Overran
                }
            };
            metrics.git_request(git_service(service), outcome);
            let killed = outcome == GitOutcome::Overran;
            // Sweep before the observer hears the moves, so a listener sees a clean repository.
            if let Some(writer) = writer {
                writer.leave(killed).await;
            }
            // Whatever the exit status, the refs are the ground truth: a client that hangs up
            // mid-report kills git with SIGPIPE after the refs already moved.
            if let Some((on_push, before)) = watch {
                let report = read_report(report_file).await;
                match list_refs(&repos, &repo_path).await {
                    Ok(after) => on_push(moved(&commands, &before, &after), report),
                    Err(err) => tracing::error!(error = &err as &dyn std::error::Error, "cannot list refs after a push"),
                }
            }
        });
        Ok(Body::from_stream(tokio_util::io::ReaderStream::new(stdout)))
    }

    /// A fresh, empty report file under `<tmp_dir>/hook-reports` for one receive-pack's hook, or
    /// `None` (warned) when it cannot be created; it is removed when dropped.
    fn report_file(&self) -> Option<tempfile::NamedTempFile> {
        let dir = self.tmp_dir.join(report::DIR_NAME);
        match tempfile::Builder::new().prefix("report-").tempfile_in(&dir) {
            Ok(file) => Some(file),
            Err(err) => {
                tracing::warn!(error = %err, "cannot create a hook report file");
                None
            }
        }
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

    /// A process slot, however long it takes to come free. The wait is bounded only as long as
    /// each holder is: a request's single git child by `max_runtime`, and the admin re-check by
    /// `max_runtime` per git child plus the rule set's CPU between them. `None` only if the
    /// slots were closed, which the engine never does.
    pub(crate) async fn slot(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.permits).acquire_owned().await.ok()
    }

    /// How long each git child may run from spawn.
    pub(crate) fn max_runtime(&self) -> Duration {
        self.max_runtime
    }

    /// A process slot, or `Busy` once `queue_wait` has passed without one. The wait is recorded
    /// either way, and a `Busy` is counted as the outcome of `service`'s request.
    async fn acquire(&self, service: GitService) -> Result<OwnedSemaphorePermit, GitError> {
        let started = tokio::time::Instant::now();
        let permit =
            tokio::time::timeout(self.queue_wait, Arc::clone(&self.permits).acquire_owned()).await;
        self.metrics.slot_wait(started.elapsed().as_secs_f64());
        match permit {
            Ok(Ok(permit)) => Ok(permit),
            _ => {
                self.metrics.git_request(service, GitOutcome::Busy);
                Err(GitError::Busy)
            }
        }
    }

    /// git with the hardening flags (fsck, and no deleting HEAD's branch for anyone) and a clean
    /// environment; the caller appends the service-specific flags and the repository path.
    fn command(&self, service: Service, v2: bool, extra_env: &[(String, String)]) -> Command {
        let mut cmd = Command::new(self.repos.git());
        cmd.arg("-c")
            .arg(format!("core.hooksPath={}", self.hooks_dir.display()))
            .arg("-c")
            .arg("receive.fsckObjects=true")
            .arg("-c")
            .arg("receive.denyDeleteCurrent=refuse")
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

/// The hook's report from `file`, which is then removed: at most `MAX_BYTES` are read, through
/// tokio so the reaper never blocks a worker. No file, or one that cannot be read (warned), is
/// the default report.
async fn read_report(file: Option<tempfile::NamedTempFile>) -> HookReport {
    let Some(file) = file else {
        return HookReport::default();
    };
    let mut bytes = Vec::new();
    let read = match file.reopen() {
        Ok(std_file) => tokio::fs::File::from_std(std_file)
            .take(report::MAX_BYTES as u64)
            .read_to_end(&mut bytes)
            .await
            .map(drop),
        Err(err) => Err(err),
    };
    drop(file);
    match read {
        Ok(()) => report::parse(&bytes),
        Err(err) => {
            tracing::warn!(error = %err, "cannot read a hook report file");
            HookReport::default()
        }
    }
}

/// Leaves the writer gate, if held, before `err` is returned: no git touched the repository, but
/// a sweep an earlier killed push deferred runs now.
async fn released(writer: Option<sweep::Writer>, err: GitError) -> GitError {
    if let Some(writer) = writer {
        writer.leave(false).await;
    }
    err
}

/// The process group `child` leads (spawned with `process_group(0)`); `None` once reaped.
pub(super) fn process_group(child: &tokio::process::Child) -> Option<rustix::process::Pid> {
    child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(rustix::process::Pid::from_raw)
}

/// A pidfd for `child`, taken while it is unreaped so it names this process for good; `None`
/// when the kernel cannot give one.
fn leader_pidfd(child: &tokio::process::Child) -> Option<std::os::fd::OwnedFd> {
    let pid = process_group(child)?;
    rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).ok()
}

/// Waits until the process behind `pidfd` exits, without reaping it.
async fn exited(pidfd: std::os::fd::OwnedFd) {
    use rustix::process::{WaitId, WaitIdOptions, waitid};
    use std::os::fd::AsFd;
    let _ = tokio::task::spawn_blocking(move || {
        loop {
            match waitid(
                WaitId::PidFd(pidfd.as_fd()),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
            ) {
                Err(rustix::io::Errno::INTR) => continue,
                // Any other error (ECHILD once reaped) counts as exited; `child.wait()` follows.
                _ => return,
            }
        }
    })
    .await;
}

/// Whether any live process other than its leader is still in `group`, read from /proc.
async fn strays_in_group(group: rustix::process::Pid) -> bool {
    let leader = group.as_raw_nonzero().get();
    tokio::task::spawn_blocking(move || {
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return false;
        };
        dir.filter_map(Result::ok)
            .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
            .filter(|pid| *pid != leader)
            .any(|pid| {
                let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                    return false;
                };
                // Fields after the command: state, ppid, pgrp.
                let Some((_, rest)) = stat.rsplit_once(") ") else {
                    return false;
                };
                let mut fields = rest.split_whitespace();
                let state = fields.next();
                let pgrp = fields.nth(1).and_then(|f| f.parse::<i32>().ok());
                pgrp == Some(leader) && !matches!(state, Some("Z" | "X"))
            })
    })
    .await
    .unwrap_or(false)
}

/// SIGKILL to every process in `group`; a no-op on `None`, ESRCH ignored.
fn kill_group(group: Option<rustix::process::Pid>) {
    use rustix::process::{Signal, kill_process_group};
    let Some(pid) = group else {
        return;
    };
    match kill_process_group(pid, Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
        Err(err) => tracing::warn!(error = %err, "cannot kill the git process group"),
    }
}

/// SIGKILLs `group`, then reaps `child`, the group's leader.
pub(super) async fn kill_and_reap(
    child: &mut tokio::process::Child,
    group: Option<rustix::process::Pid>,
) {
    kill_group(group);
    let _ = child.wait().await;
}

/// The metrics label of an RPC to `service`.
fn git_service(service: Service) -> GitService {
    match service {
        Service::UploadPack => GitService::UploadPack,
        Service::ReceivePack => GitService::ReceivePack,
    }
}

/// Kills and reaps git at the run limit, logs the one warning, and returns `Overran`.
async fn overran(
    child: &mut tokio::process::Child,
    group: Option<rustix::process::Pid>,
    service: Service,
    limit: Duration,
) -> GitError {
    kill_and_reap(child, group).await;
    tracing::warn!(
        service = service.name(),
        limit = ?limit,
        "git ran past its run limit; killed its process group"
    );
    GitError::Overran {
        service: service.name(),
        limit,
    }
}

/// Kills `group` when dropped unless disarmed, so a cancelled caller (the client hung up)
/// takes down git's whole group, not just the leader `kill_on_drop` reaches.
pub(super) struct GroupKill(pub(super) Option<rustix::process::Pid>);

impl GroupKill {
    /// Stops the drop from signalling. Call it once git is reaped or its group already killed,
    /// so a reused pid is never signalled for the first time.
    pub(super) fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        kill_group(self.0);
    }
}

/// Parses the receive-pack command list off the spool and hands the file back rewound. Over the
/// cap is `TooManyCommands`; any other parse failure is logged and counts as no commands.
async fn read_commands(
    mut file: std::fs::File,
) -> Result<(std::fs::File, Vec<RefCommand>), GitError> {
    use std::io::Seek;
    let (file, parsed) = tokio::task::spawn_blocking(move || {
        let parsed = parse_commands(std::io::BufReader::new(&mut file));
        file.rewind().map(|()| (file, parsed))
    })
    .await
    // The join error is never carried on: a panic payload is arbitrary text and can name paths.
    .map_err(|err| {
        let what = if err.is_panic() {
            "panicked"
        } else {
            "was cancelled"
        };
        GitError::Spawn(std::io::Error::other(format!(
            "parsing the command list {what}"
        )))
    })?
    .map_err(GitError::Spawn)?;
    match parsed {
        Ok(commands) => Ok((file, commands)),
        Err(CommandError::TooMany { limit }) => Err(GitError::TooManyCommands { limit }),
        Err(err) => {
            tracing::warn!(
                error = &err as &dyn std::error::Error,
                "cannot parse the receive-pack command list"
            );
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
    #[error("request body could not be read")]
    Body(#[source] std::io::Error),
    /// The spool file or the child could not be set up: 500 `internal`.
    #[error("cannot run git")]
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
        /// `Service::name()` of the killed child, or `git-for-each-ref` for a ref listing.
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
            // The read error is the useful part for the client, and the variant's own text omits it.
            GitError::Body(ref source) => ApiError::invalid_request(format!("{err}: {source}")),
            GitError::BadEncoding(_) | GitError::SlowBody(_) | GitError::TooManyCommands { .. } => {
                ApiError::invalid_request(err.to_string())
            }
            GitError::Spawn(_)
            | GitError::Failed { .. }
            | GitError::HookMissing { .. }
            | GitError::Overran { .. } => {
                tracing::error!(error = &err as &dyn std::error::Error, "git error");
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
        engine_with_git(dir, limits, PathBuf::from("git")).await
    }

    /// As `engine_in`, spawning `git` for every git child, receive-pack and ref listings included.
    pub(super) async fn engine_with_git(
        dir: &Path,
        limits: GitLimits,
        git: PathBuf,
    ) -> (GitEngine, PathBuf) {
        let repos = Repos::new(dir.join("repos"), git);
        let repo = repos
            .init(
                &Slug::parse("core").unwrap(),
                &Slug::parse("api").unwrap(),
                "main",
            )
            .await
            .unwrap();
        std::fs::create_dir_all(dir.join("tmp").join("hook-reports")).unwrap();
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
        std::fs::create_dir_all(hooks).unwrap();
        write_script(&hooks.join("pre-receive"), "#!/bin/sh\nexit 0\n", mode);
    }

    /// `text` at `path` with `mode`, written by a child `sh` to a temporary name and renamed into
    /// place, so this process never holds a write fd a sibling thread's fork could inherit and
    /// turn the exec into ETXTBSY.
    fn write_script(path: &Path, text: &str, mode: u32) {
        use std::io::Write;
        let mut child = std::process::Command::new("sh")
            .args([
                "-c",
                r#"cat > "$1.tmp" && chmod "$2" "$1.tmp" && mv -f "$1.tmp" "$1""#,
                "sh",
            ])
            .arg(path)
            .arg(format!("{mode:o}"))
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // Close stdin and reap the child before judging either step, so no failure strands it.
        let written = child.stdin.take().unwrap().write_all(text.as_bytes());
        let status = child.wait();
        match (written, status) {
            (Ok(()), Ok(status)) if status.success() => {}
            outcome => panic!("writing {}: {outcome:?}", path.display()),
        }
    }

    #[test]
    fn a_written_script_execs_while_other_threads_fork() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let forkers: Vec<_> = (0..4)
            .map(|_| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let _ = std::process::Command::new("true").status();
                    }
                })
            })
            .collect();
        let runs = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (0..200)
                .map(|i| {
                    let path = dir.path().join(format!("script-{i}"));
                    write_script(&path, "#!/bin/sh\nexit 0\n", 0o755);
                    std::process::Command::new(&path).status()
                })
                .collect::<Vec<std::io::Result<std::process::ExitStatus>>>()
        }));
        // Results are collected before asserting, and a panic while writing is held until the
        // forkers are stopped and joined, so no failure leaves them running.
        stop.store(true, Ordering::Relaxed);
        for forker in forkers {
            forker.join().unwrap();
        }
        let runs = runs.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        let busy = runs
            .iter()
            .filter(|run| {
                run.as_ref()
                    .is_err_and(|err| err.kind() == std::io::ErrorKind::ExecutableFileBusy)
            })
            .count();
        assert_eq!(busy, 0, "runs failed with ETXTBSY");
        for run in runs {
            assert!(run.unwrap().success());
        }
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
        write_script(path, text, 0o755);
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
            Box::new(move |moves, _report| {
                let _ = tx.send(moves);
            }),
            rx,
        )
    }

    #[tokio::test]
    async fn a_refusing_hook_reaches_the_observer_through_the_report_file() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, repo) = engine_in(dir.path(), GitLimits::default()).await;
        write_script(
            &dir.path().join("hooks").join("pre-receive"),
            "#!/bin/sh\ncat >/dev/null\nprintf 'secret_env_file env_file\\n' > \"$POTLUCK_HOOK_REPORT\"\nexit 1\n",
            0o755,
        );
        let (body, _tip) = push_body(dir.path());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let on_push: PushObserver = Box::new(move |moves, report| {
            let _ = tx.send((moves, report));
        });
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
        let (moves, report) = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .unwrap()
            .unwrap();
        assert!(moves.is_empty(), "nothing landed");
        assert!(report.refused());
        assert_eq!(
            report.refusals(),
            [("secret_env_file", "env_file".to_owned())]
        );
        engine.tasks.close();
        engine.tasks.wait().await;
        let left = std::fs::read_dir(dir.path().join("tmp").join("hook-reports"))
            .unwrap()
            .count();
        assert_eq!(left, 0, "the report file is removed");
    }

    #[tokio::test]
    async fn the_hook_report_variable_is_removed_when_no_report_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, repo) = engine_in(dir.path(), GitLimits::default()).await;
        std::fs::remove_dir(dir.path().join("tmp").join("hook-reports")).unwrap();
        let seen = dir.path().join("seen");
        write_script(
            &dir.path().join("hooks").join("pre-receive"),
            &format!(
                "#!/bin/sh\ncat >/dev/null\nprintf '%s' \"${{POTLUCK_HOOK_REPORT-unset}}\" > '{}'\nexit 0\n",
                seen.display()
            ),
            0o755,
        );
        let (body, _tip) = push_body(dir.path());
        let planted = vec![(
            crate::secret_check::REPORT_ENV.to_owned(),
            dir.path().join("planted").display().to_string(),
        )];
        let out = engine
            .rpc(
                Service::ReceivePack,
                &repo,
                false,
                None,
                &planted,
                Body::from(body),
                None,
            )
            .await
            .unwrap();
        let _ = collect(out).await;
        assert_eq!(std::fs::read_to_string(&seen).unwrap(), "unset");
    }

    #[tokio::test]
    async fn an_accepting_hook_gives_an_unrefused_report() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, repo) = engine_in(dir.path(), GitLimits::default()).await;
        let (body, _tip) = push_body(dir.path());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let on_push: PushObserver = Box::new(move |moves, report| {
            let _ = tx.send((moves, report));
        });
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
        let (moves, report) = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(moves.len(), 1);
        assert!(!report.refused());
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
            let refs = list_refs(&engine.repos, &repo).await.unwrap();
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

    /// SIGKILLs, when dropped, the `sleep 30` whose pid a hook wrote to the held path, so a test
    /// that panics never leaves it running; armed before the hook runs, it reads the file in drop.
    struct KillsSleeper(PathBuf);

    impl Drop for KillsSleeper {
        fn drop(&mut self) {
            use rustix::process::{Pid, Signal, kill_process};
            let Some(pid) = std::fs::read_to_string(&self.0)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
            else {
                return;
            };
            // Only a live process still running the hook's sleep, so a reused pid is never hit.
            let ours = std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|cmd| cmd == b"sleep\x0030\x00");
            if ours
                && !is_dead(pid)
                && let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw)
            {
                let _ = kill_process(pid, Signal::KILL);
            }
        }
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

    #[tokio::test]
    async fn a_push_killed_holding_ref_locks_leaves_none_and_the_retry_lands() {
        use crate::git::push::{RefMove, ZERO_SHA};
        let dir = tempfile::tempdir().unwrap();
        let short = limits(1, 100).with_max_runtime(Duration::from_millis(1500));
        let (engine, repo) = engine_in(dir.path(), short).await;
        // A pack, not loose objects, so receive-pack also writes its `.keep`.
        let set = std::process::Command::new("git")
            .args([
                "--git-dir",
                repo.to_str().unwrap(),
                "config",
                "receive.unpackLimit",
                "1",
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(set.success());
        // `prepared` fires with every ref lock of the transaction held.
        let marker = dir.path().join("prepared.seen");
        let hook = dir.path().join("hooks").join("reference-transaction");
        write_hook_file(
            &hook,
            &format!(
                "#!/bin/sh\ncat >/dev/null\nif [ \"$1\" = prepared ]; then echo locked > '{}'; exec sleep 30; fi\n",
                marker.display()
            ),
        );
        let (body, tip) = push_body(dir.path());
        assert!(
            push_once(&engine, &repo, body.clone()).await.is_empty(),
            "killed"
        );
        line_from(&marker).await;
        assert_eq!(debris(&repo), Vec::<String>::new());
        std::fs::remove_file(&hook).unwrap();
        assert_eq!(
            push_once(&engine, &repo, body).await,
            vec![RefMove::new("refs/heads/main", ZERO_SHA, &tip)]
        );
    }

    #[tokio::test]
    async fn a_killed_push_beside_a_live_one_is_swept_only_after_the_live_one_ends() {
        use crate::git::push::{RefMove, ZERO_SHA};
        let dir = tempfile::tempdir().unwrap();
        let limit = limits(2, 100).with_max_runtime(Duration::from_secs(8));
        let (engine, repo) = engine_in(dir.path(), limit).await;
        let (doomed_at, live_at, go) = (
            dir.path().join("doomed.path"),
            dir.path().join("live.path"),
            dir.path().join("go"),
        );
        // The doomed push hangs to the limit; the live one waits for the test's go-ahead.
        write_hook_file(
            &dir.path().join("hooks").join("pre-receive"),
            &format!(
                "#!/bin/sh\nread old new ref\ncase \"$ref\" in\n\
                 refs/heads/doomed) echo \"$GIT_QUARANTINE_PATH\" > '{d}'; exec sleep 30 ;;\n\
                 *) echo \"$GIT_QUARANTINE_PATH\" > '{l}'; i=0\n\
                 while [ ! -e '{g}' ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done ;;\nesac\n",
                d = doomed_at.display(),
                l = live_at.display(),
                g = go.display()
            ),
        );
        let (doomed_body, _) = push_body_for(dir.path(), "doomed");
        let (live_body, live_tip) = push_body_for(dir.path(), "main");
        let engine = std::sync::Arc::new(engine);
        let doomed = tokio::spawn({
            let (engine, repo) = (std::sync::Arc::clone(&engine), repo.clone());
            async move { push_once(&engine, &repo, doomed_body).await }
        });
        let doomed_q = line_from(&doomed_at).await;
        // Start the live push halfway through the doomed one's limit, so it is inside the
        // repository when the doomed one is killed and still well inside its own limit.
        tokio::time::sleep(Duration::from_secs(4)).await;
        let live = tokio::spawn({
            let (engine, repo) = (std::sync::Arc::clone(&engine), repo.clone());
            async move { push_once(&engine, &repo, live_body).await }
        });
        let live_q = line_from(&live_at).await;
        assert!(
            doomed.await.unwrap().is_empty(),
            "the doomed push was killed"
        );
        assert!(
            Path::new(&live_q).exists(),
            "the live push keeps its quarantine"
        );
        assert!(
            Path::new(&doomed_q).exists(),
            "the sweep waits for the live push"
        );
        std::fs::write(&go, "").unwrap();
        assert_eq!(
            live.await.unwrap(),
            vec![RefMove::new("refs/heads/main", ZERO_SHA, &live_tip)]
        );
        assert_eq!(
            debris(&repo),
            Vec::<String>::new(),
            "swept once the live push left"
        );
    }

    #[tokio::test]
    async fn a_hung_push_holds_the_git_tasks_for_two_run_limits_and_leaves_nothing_running() {
        let dir = tempfile::tempdir().unwrap();
        let limit = Duration::from_millis(1500);
        let (hook_pid, list_pid, marker) = (
            dir.path().join("hook.pid"),
            dir.path().join("list.pid"),
            dir.path().join("receive.started"),
        );
        // The real git for everything except a ref listing after the hook has started, which
        // hangs: the reaper's listing, not the one taken before receive-pack.
        let wrapper = dir.path().join("git-wrapper");
        write_hook_file(
            &wrapper,
            &format!(
                "#!/bin/sh\ncase \"$*\" in *for-each-ref*) if [ -e '{m}' ]; then echo $$ > '{p}'; exec sleep 30; fi ;; esac\nexec git \"$@\"\n",
                m = marker.display(),
                p = list_pid.display()
            ),
        );
        let _hook = KillsSleeper(hook_pid.clone());
        let _list = KillsSleeper(list_pid.clone());
        let (engine, repo) =
            engine_with_git(dir.path(), limits(1, 100).with_max_runtime(limit), wrapper).await;
        write_hook_file(
            &dir.path().join("hooks").join("pre-receive"),
            &format!(
                "#!/bin/sh\necho $$ > '{p}'\n: > '{m}'\nexec sleep 30\n",
                p = hook_pid.display(),
                m = marker.display()
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
            .expect("the body ends once receive-pack's group is killed");
        // What `Running::shutdown` waits on: the reaper ends only after the listing's own limit.
        engine.tasks().close();
        tokio::time::timeout(Duration::from_secs(20), engine.tasks().wait())
            .await
            .expect("the reaper ends");
        let held = started.elapsed();
        assert!(
            held >= 2 * limit,
            "receive-pack and the listing each ran a full limit: {held:?}"
        );
        assert!(
            held < 3 * limit + Duration::from_secs(3),
            "bounded near two limits: {held:?}"
        );
        assert!(moved.await.is_err(), "a failed listing reports no moves");
        for pid in [read_pid(&hook_pid), read_pid(&list_pid)] {
            assert!(eventually(5, || is_dead(pid)).await, "{pid} was killed");
        }
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
        let refs = list_refs(&engine.repos, &repo).await.unwrap();
        assert!(!refs.contains_key("refs/heads/main"), "nothing was pushed");
    }

    #[tokio::test]
    async fn rpc_kills_a_hook_descendant_holding_stderr_after_receive_pack_exits() {
        let dir = tempfile::tempdir().unwrap();
        let short = limits(1, 100).with_max_runtime(Duration::from_millis(1500));
        let (engine, repo) = engine_in(dir.path(), short).await;
        let pidfile = dir.path().join("descendant.pid");
        // The hook accepts at once and leaves a child holding stderr, which the push body's
        // missing side-band makes receive-pack's own stderr pipe.
        write_hook_file(
            &dir.path().join("hooks").join("pre-receive"),
            &format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nexit 0\n",
                pidfile.display()
            ),
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
        let _ = tokio::time::timeout(Duration::from_secs(10), collect(out))
            .await
            .expect("receive-pack answers without waiting for the descendant");
        let moves = tokio::time::timeout(Duration::from_secs(10), moved)
            .await
            .expect("the observer is called once the limit passes")
            .expect("not dropped");
        assert_eq!(
            moves.len(),
            1,
            "receive-pack exited after landing the push: {moves:?}"
        );
        assert_eq!(moves[0].new_sha(), tip);
        assert!(
            eventually(10, || engine.permits.available_permits() == 1).await,
            "the slot came back"
        );
        let pid = read_pid(&pidfile);
        assert!(
            eventually(5, || is_dead(pid)).await,
            "the descendant {pid} was killed at the limit, not left to its 30 s"
        );
    }

    #[tokio::test]
    async fn rpc_kills_a_hook_descendant_that_closed_stderr_at_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let short = limits(1, 100).with_max_runtime(Duration::from_millis(1500));
        let (engine, repo) = engine_in(dir.path(), short).await;
        let pidfile = dir.path().join("descendant.pid");
        let _sleeper = KillsSleeper(pidfile.clone());
        // The hook accepts at once and leaves a child with every stdio stream closed, so nothing
        // ties it to receive-pack's pipes once receive-pack exits.
        write_hook_file(
            &dir.path().join("hooks").join("pre-receive"),
            &format!(
                "#!/bin/sh\nsleep 30 </dev/null >/dev/null 2>&1 &\necho $! > '{}'\nexit 0\n",
                pidfile.display()
            ),
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
        let _ = tokio::time::timeout(Duration::from_secs(10), collect(out))
            .await
            .expect("receive-pack answers without waiting for the descendant");
        let moves = tokio::time::timeout(Duration::from_secs(10), moved)
            .await
            .expect("the observer is called by the limit")
            .expect("not dropped");
        assert_eq!(moves.len(), 1, "the push landed: {moves:?}");
        assert_eq!(moves[0].new_sha(), tip);
        assert!(
            eventually(10, || engine.permits.available_permits() == 1).await,
            "the slot came back"
        );
        let pid = read_pid(&pidfile);
        assert!(
            eventually(5, || is_dead(pid)).await,
            "the descendant {pid} was killed at the limit, not left to its 30 s"
        );
    }

    // Detached auto maintenance calls setsid(2) and so leaves git's group: the run limit must
    // neither wait for it nor kill it.
    #[tokio::test]
    async fn a_hook_descendant_that_leaves_the_group_is_neither_awaited_nor_killed() {
        let dir = tempfile::tempdir().unwrap();
        let short = limits(1, 100).with_max_runtime(Duration::from_millis(1500));
        let (engine, repo) = engine_in(dir.path(), short).await;
        let pidfile = dir.path().join("detached.pid");
        // The detached sleep is outside every group the engine kills; the guard ends it.
        let _sleeper = KillsSleeper(pidfile.clone());
        write_hook_file(
            &dir.path().join("hooks").join("pre-receive"),
            &format!(
                "#!/bin/sh\nsetsid sleep 30 </dev/null >/dev/null 2>&1 &\necho $! > '{}'\nexit 0\n",
                pidfile.display()
            ),
        );
        let (body, _tip) = push_body(dir.path());
        let started = std::time::Instant::now();
        let moves = push_once(&engine, &repo, body).await;
        let elapsed = started.elapsed();
        let pid = read_pid(&pidfile);
        tokio::time::sleep(Duration::from_secs(2)).await;
        let alive = !is_dead(pid);
        assert_eq!(moves.len(), 1, "the push landed: {moves:?}");
        assert!(
            elapsed < Duration::from_millis(1500),
            "the push did not wait for the detached process: {elapsed:?}"
        );
        assert!(alive, "the detached process {pid} outlived the limit");
    }

    // A descendant that ends on its own inside the limit holds the push until it ends, unkilled.
    #[tokio::test]
    async fn a_push_waits_for_a_hook_descendant_that_ends_inside_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let limit = limits(1, 100).with_max_runtime(Duration::from_secs(20));
        let (engine, repo) = engine_in(dir.path(), limit).await;
        let done = dir.path().join("descendant.done");
        write_hook_file(
            &dir.path().join("hooks").join("pre-receive"),
            &format!(
                "#!/bin/sh\n(sleep 1; echo ok > '{}') </dev/null >/dev/null 2>&1 &\nexit 0\n",
                done.display()
            ),
        );
        let (body, _tip) = push_body(dir.path());
        let started = std::time::Instant::now();
        let moves = push_once(&engine, &repo, body).await;
        assert!(
            done.exists(),
            "the observer heard the push only once the descendant ended"
        );
        assert_eq!(moves.len(), 1, "the push landed: {moves:?}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the push ended with its descendant, not at the 20 s limit: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn an_advertisement_whose_git_exits_is_killed_with_a_child_holding_stdout() {
        let dir = tempfile::tempdir().unwrap();
        // A stand-in git that exits at once, leaving a child that holds both pipes open.
        let pidfile = dir.path().join("child.pid");
        let fake = dir.path().join("fake-git");
        write_script(
            &fake,
            &format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nexit 0\n",
                pidfile.display()
            ),
            0o755,
        );
        std::fs::create_dir_all(dir.path().join("tmp")).unwrap();
        write_stub_hook(&dir.path().join("hooks"), 0o755);
        let engine = GitEngine::new(
            Repos::new(dir.path().join("repos"), fake),
            dir.path().join("hooks"),
            dir.path().join("tmp"),
            1024,
            limits(1, 100).with_max_runtime(Duration::from_millis(1500)),
        );
        let err = engine
            .advertise(
                Service::UploadPack,
                &dir.path().join("repos").join("x.git"),
                false,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                GitError::Overran {
                    service: "git-upload-pack",
                    ..
                }
            ),
            "got {err:?}"
        );
        assert_eq!(engine.permits.available_permits(), 1, "the slot is back");
        let pid = read_pid(&pidfile);
        assert!(
            eventually(5, || is_dead(pid)).await,
            "the child {pid} was killed at the limit, not left to its 30 s"
        );
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
        let dir = tempfile::tempdir().unwrap();
        // A stand-in git that never answers and leaves a child of its own behind.
        let pidfile = dir.path().join("child.pid");
        let fake = dir.path().join("fake-git");
        write_script(
            &fake,
            &format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nwait\n",
                pidfile.display()
            ),
            0o755,
        );
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

    #[tokio::test]
    async fn a_client_hanging_up_on_an_advertisement_kills_its_whole_group() {
        use tokio::io::AsyncWriteExt;
        let dir = tempfile::tempdir().unwrap();
        // A stand-in git that records its pid, leaves a child of its own and never answers.
        let leader_file = dir.path().join("leader.pid");
        let child_file = dir.path().join("child.pid");
        let fake = dir.path().join("fake-git");
        write_script(
            &fake,
            &format!(
                "#!/bin/sh\necho $$ > '{}'\nsleep 30 &\necho $! > '{}'\nwait\n",
                leader_file.display(),
                child_file.display()
            ),
            0o755,
        );
        std::fs::create_dir_all(dir.path().join("tmp")).unwrap();
        write_stub_hook(&dir.path().join("hooks"), 0o755);
        // The run limit is far past the test's waits, so only the hangup can end git.
        let engine = Arc::new(GitEngine::new(
            Repos::new(dir.path().join("repos"), fake),
            dir.path().join("hooks"),
            dir.path().join("tmp"),
            1024,
            limits(1, 100).with_max_runtime(Duration::from_secs(20)),
        ));
        let repo = dir.path().join("repos").join("x.git");
        let app = axum::Router::new().route(
            "/",
            axum::routing::get({
                let engine = Arc::clone(&engine);
                move || {
                    let (engine, repo) = (Arc::clone(&engine), repo.clone());
                    async move {
                        let _ = engine.advertise(Service::UploadPack, &repo, false).await;
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        assert!(
            eventually(10, || std::fs::read_to_string(&child_file)
                .is_ok_and(|t| t.ends_with('\n')))
            .await,
            "the stand-in git started its child"
        );
        let (leader, child) = (read_pid(&leader_file), read_pid(&child_file));
        drop(client);

        assert!(
            eventually(5, || is_dead(leader)).await,
            "the hangup cancelled the advertisement and killed git {leader}"
        );
        assert!(
            eventually(5, || is_dead(child)).await,
            "git's child {child} died with its group, not left to its 30 s"
        );
        assert!(
            eventually(5, || engine.permits.available_permits() == 1).await,
            "the slot is back"
        );
    }

    // A push takes its slot before the writer gate. Were the order swapped, a push left waiting
    // for a slot would sit inside the gate when a killed push leaves, take over its sweep, and
    // then fail `Busy` without running it.
    #[tokio::test]
    async fn a_push_that_never_gets_a_slot_does_not_strand_a_killed_push_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let limit = limits(1, 3000).with_max_runtime(Duration::from_millis(1500));
        let (engine, repo) = engine_in(dir.path(), limit).await;
        let doomed_at = dir.path().join("doomed.path");
        write_hook_file(
            &dir.path().join("hooks").join("pre-receive"),
            &format!(
                "#!/bin/sh\necho \"$GIT_QUARANTINE_PATH\" > '{}'\nexec sleep 30\n",
                doomed_at.display()
            ),
        );
        let (doomed_body, _) = push_body_for(dir.path(), "doomed");
        let (late_body, _) = push_body_for(dir.path(), "late");
        let engine = Arc::new(engine);
        let doomed = tokio::spawn({
            let (engine, repo) = (Arc::clone(&engine), repo.clone());
            async move { push_once(&engine, &repo, doomed_body).await }
        });
        let doomed_q = line_from(&doomed_at).await;
        // Queued ahead of the late push, this takes the slot the killed push frees and holds
        // it until the late push has given up.
        let ahead = tokio::spawn(Arc::clone(&engine.permits).acquire_owned());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let late = engine
            .rpc(
                Service::ReceivePack,
                &repo,
                false,
                None,
                &[],
                Body::from(late_body),
                None,
            )
            .await;
        assert!(matches!(late, Err(GitError::Busy)), "got {late:?}");
        assert!(
            doomed.await.unwrap().is_empty(),
            "the doomed push was killed"
        );
        assert!(
            !Path::new(&doomed_q).exists(),
            "the killed push swept its own quarantine"
        );
        assert_eq!(
            debris(&repo),
            Vec::<String>::new(),
            "nothing is left to sweep"
        );
        drop(ahead.await.unwrap().unwrap());
    }

    // A push that fails after entering the writer gate but before git starts must still run a
    // sweep an earlier killed push deferred, once the repository is quiet.
    #[tokio::test]
    async fn a_push_failing_before_git_starts_runs_the_deferred_sweep() {
        for observe in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (_, repo) = engine_in(dir.path(), limits(1, 100)).await;
            // The same layout with a git binary that does not exist, so both `list_refs` and the
            // spawn fail after the gate is entered.
            let engine = GitEngine::new(
                Repos::new(
                    dir.path().join("repos"),
                    PathBuf::from("/nonexistent/potluck-test-git"),
                ),
                dir.path().join("hooks"),
                dir.path().join("tmp"),
                1024 * 1024,
                limits(1, 100),
            );
            std::fs::write(repo.join("refs/heads/stale.lock"), "x").unwrap();
            // A killed push leaves while another writer is inside, so its sweep is deferred; the
            // other writer is then dropped, leaving the repository dirty and quiet.
            let held = engine.writers.enter(&repo).await;
            let killed = engine.writers.enter(&repo).await;
            assert!(
                !killed.leave(true).await,
                "deferred while another writer is inside"
            );
            drop(held);
            let (on_push, _moved) = observer();
            let err = engine
                .rpc(
                    Service::ReceivePack,
                    &repo,
                    false,
                    None,
                    &[],
                    Body::from("0000"),
                    observe.then_some(on_push),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(err, GitError::Spawn(_)),
                "observe={observe}: {err:?}"
            );
            assert!(
                !repo.join("refs/heads/stale.lock").exists(),
                "observe={observe}: the failed push ran the deferred sweep"
            );
        }
    }

    /// Every log line from any thread since the first call. Process-wide, since a failed
    /// re-check is logged by `log_accepted_relaxations` on its `spawn_blocking` thread, which a
    /// thread-scoped subscriber never sees; tests tell their lines apart by workspace id.
    fn global_logs() -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
        use std::sync::{Arc, Mutex, OnceLock};
        static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
        Arc::clone(LOGS.get_or_init(|| {
            let logs = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&logs);
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::INFO)
                .with_ansi(false)
                .with_writer(move || SharedSink(Arc::clone(&sink)))
                .finish();
            tracing::subscriber::set_global_default(subscriber).unwrap();
            logs
        }))
    }

    struct SharedSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// An engine whose git is the script `body` and whose limits are `limits`; no repository.
    fn recheck_engine(dir: &Path, limits: GitLimits, body: &str) -> GitEngine {
        let git = dir.join("recheck-git");
        write_script(&git, &format!("#!/bin/sh\n{body}"), 0o755);
        GitEngine::new(
            Repos::new(dir.join("repos"), git),
            dir.join("hooks"),
            dir.join("tmp"),
            1024 * 1024,
            limits,
        )
    }

    /// An update with a baseline, so the re-check's first step is a git call.
    fn moved_update() -> Vec<crate::secret_check::git::RefUpdate> {
        let line = format!("{} {} refs/heads/main", "a".repeat(40), "b".repeat(40));
        vec![crate::secret_check::git::RefUpdate::parse_line(line.as_bytes()).unwrap()]
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_admin_recheck_waits_for_a_process_slot() {
        let dir = tempfile::tempdir().unwrap();
        let db = relaxation_db(dir.path(), 1).await;
        let ran = dir.path().join("ran.log");
        let body = format!("echo \"$1\" >> '{}'\nexit 128\n", ran.display());
        let engine = Arc::new(recheck_engine(dir.path(), limits(1, 100), &body));
        let held = engine.slot().await.unwrap();
        let rules = crate::kinds::by_name("apinata").unwrap();
        let task = {
            let engine = Arc::clone(&engine);
            let repo = dir.path().join("repo");
            tokio::spawn(async move {
                crate::api::pushes::recheck(
                    &engine,
                    &db,
                    &crate::git::access::PushContext::new(1, "admin"),
                    Some(rules),
                    repo,
                    moved_update(),
                )
                .await;
            })
        };
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!ran.exists(), "no git runs while the only slot is held");
        drop(held);
        tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .unwrap()
            .unwrap();
        assert!(ran.exists(), "git runs once the slot frees");
    }

    #[test]
    fn a_git_server_fault_logs_its_cause() {
        let logs = global_logs();
        let _ = ApiError::from(GitError::Spawn(std::io::Error::other("spool-cause-5306")));
        let text = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
        assert!(
            text.lines()
                .any(|l| l.contains("git error") && l.contains("spool-cause-5306")),
            "{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_admin_recheck_stops_at_the_run_limit_and_warns() {
        const WORKSPACE: i64 = 919_198;
        let logs = global_logs();
        let dir = tempfile::tempdir().unwrap();
        let db = relaxation_db(dir.path(), WORKSPACE).await;
        let short = limits(1, 100).with_max_runtime(Duration::from_secs(1));
        let engine = recheck_engine(dir.path(), short, "exec sleep 30\n");
        let rules = crate::kinds::by_name("apinata").unwrap();
        let started = std::time::Instant::now();
        let repo = dir.path().join("repo");
        let done = tokio::time::timeout(
            Duration::from_secs(20),
            crate::api::pushes::recheck(
                &engine,
                &db,
                &crate::git::access::PushContext::new(WORKSPACE, "admin"),
                Some(rules),
                repo,
                moved_update(),
            ),
        )
        .await;
        assert!(done.is_ok(), "the re-check returned");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(
            text.lines()
                .any(|l| l.contains("could not re-check an accepted admin push")
                    && l.contains(&format!("workspace_id={WORKSPACE}"))),
            "{text}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_override_recheck_without_a_rule_set_logs_a_default_branch_delete() {
        const WORKSPACE: i64 = 919_199;
        let logs = global_logs();
        let dir = tempfile::tempdir().unwrap();
        let db = relaxation_db(dir.path(), WORKSPACE).await;
        // A delete needs no git call, so a git that always fails proves none ran.
        let engine = recheck_engine(dir.path(), limits(1, 100), "exit 128\n");
        let line = format!("{} {} refs/heads/main", "b".repeat(40), "0".repeat(40));
        let delete =
            vec![crate::secret_check::git::RefUpdate::parse_line(line.as_bytes()).unwrap()];
        let push = crate::git::access::PushContext::new(WORKSPACE, "bob")
            .override_id(Some(5))
            .default_branch("main");
        crate::api::pushes::recheck(&engine, &db, &push, None, dir.path().join("repo"), delete)
            .await;
        let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        let line = text
            .lines()
            .find(|l| {
                l.contains("default branch rewritten")
                    && l.contains(&format!("workspace_id={WORKSPACE}"))
            })
            .unwrap_or_else(|| panic!("{text}"));
        for needle in ["override_id=5", "bob", "branch_delete"] {
            assert!(line.contains(needle), "{needle} in {line}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_override_recheck_logs_a_ref_outside_heads_and_tags() {
        const WORKSPACE: i64 = 919_200;
        let logs = global_logs();
        let dir = tempfile::tempdir().unwrap();
        let db = relaxation_db(dir.path(), WORKSPACE).await;
        // Ref names alone decide, so a git that always fails proves none ran.
        let engine = recheck_engine(dir.path(), limits(1, 100), "exit 128\n");
        let line = format!("{} {} refs/notes/x", "0".repeat(40), "b".repeat(40));
        let create =
            vec![crate::secret_check::git::RefUpdate::parse_line(line.as_bytes()).unwrap()];
        let push = crate::git::access::PushContext::new(WORKSPACE, "bob")
            .override_id(Some(5))
            .default_branch("main");
        crate::api::pushes::recheck(&engine, &db, &push, None, dir.path().join("repo"), create)
            .await;
        let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        let line = text
            .lines()
            .find(|l| {
                l.contains("ref outside heads and tags written")
                    && l.contains(&format!("workspace_id={WORKSPACE}"))
            })
            .unwrap_or_else(|| panic!("{text}"));
        for needle in ["override_id=5", "bob", "refs/notes/x"] {
            assert!(line.contains(needle), "{needle} in {line}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_override_recheck_logs_a_tag_move() {
        const WORKSPACE: i64 = 919_201;
        let logs = global_logs();
        let dir = tempfile::tempdir().unwrap();
        let db = relaxation_db(dir.path(), WORKSPACE).await;
        // Raw values alone decide, so a git that always fails proves none ran.
        let engine = recheck_engine(dir.path(), limits(1, 100), "exit 128\n");
        let line = format!("{} {} refs/tags/v1", "a".repeat(40), "b".repeat(40));
        let moved = vec![crate::secret_check::git::RefUpdate::parse_line(line.as_bytes()).unwrap()];
        let push = crate::git::access::PushContext::new(WORKSPACE, "bob")
            .override_id(Some(5))
            .default_branch("main");
        crate::api::pushes::recheck(&engine, &db, &push, None, dir.path().join("repo"), moved)
            .await;
        let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        let line = text
            .lines()
            .find(|l| {
                l.contains("tag rewritten") && l.contains(&format!("workspace_id={WORKSPACE}"))
            })
            .unwrap_or_else(|| panic!("{text}"));
        for needle in ["override_id=5", "bob", "refs/tags/v1", "tag_move"] {
            assert!(line.contains(needle), "{needle} in {line}");
        }
    }

    /// A migrated database with user `bob`, workspace `workspace` as `core/api`, and override 5
    /// for bob on it.
    async fn relaxation_db(dir: &Path, workspace: i64) -> crate::db::Db {
        let db = crate::db::Db::open(&dir.join("server.db")).unwrap();
        db.migrate().await.unwrap();
        db.call(move |c| {
            c.execute_batch(&format!(
                "INSERT INTO users (id, username, display_name, created_at) VALUES (1, 'bob', 'Bob', '2026-01-01T00:00:00Z');
                 INSERT INTO teams (id, slug, name) VALUES (1, 'core', 'Core');
                 INSERT INTO workspaces (id, team_id, slug, name, kind, default_branch, import_pending, created_at)
                     VALUES ({workspace}, 1, 'api', 'API', 'apinata', 'main', 0, '2026-01-01T00:00:00Z');
                 INSERT INTO overrides (id, workspace_id, for_user_id, reason, armed_by, armed_at)
                     VALUES (5, {workspace}, 1, 'fix', 'system', '2026-01-01T00:00:00Z');"
            ))?;
            Ok(())
        })
        .await
        .unwrap();
        db
    }

    /// The `push.relaxation` details, oldest first.
    async fn relaxation_details(db: &crate::db::Db) -> Vec<serde_json::Value> {
        let filter = crate::db::audit::AuditFilter::new().action("push.relaxation");
        let mut rows = db.audit(filter).await.unwrap();
        rows.reverse();
        rows.iter()
            .map(|r| serde_json::from_str(r.detail().unwrap()).unwrap())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_override_recheck_records_a_default_branch_delete_naming_the_override() {
        const WORKSPACE: i64 = 919_202;
        let dir = tempfile::tempdir().unwrap();
        let db = relaxation_db(dir.path(), WORKSPACE).await;
        // A delete needs no git call, so a git that always fails proves none ran.
        let engine = recheck_engine(dir.path(), limits(1, 100), "exit 128\n");
        let (b, zero) = ("b".repeat(40), "0".repeat(40));
        let line = format!("{b} {zero} refs/heads/main");
        let delete =
            vec![crate::secret_check::git::RefUpdate::parse_line(line.as_bytes()).unwrap()];
        let push = crate::git::access::PushContext::new(WORKSPACE, "bob")
            .override_id(Some(5))
            .default_branch("main");
        crate::api::pushes::recheck(&engine, &db, &push, None, dir.path().join("repo"), delete)
            .await;
        assert_eq!(
            relaxation_details(&db).await,
            [serde_json::json!({
                "ref": "refs/heads/main", "old": b, "new": zero, "rule": "branch_delete", "override": 5
            })]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_admin_recheck_records_a_foreign_ref_with_no_override() {
        const WORKSPACE: i64 = 919_203;
        let dir = tempfile::tempdir().unwrap();
        let db = relaxation_db(dir.path(), WORKSPACE).await;
        // Ref names alone decide, so a git that always fails proves none ran.
        let engine = recheck_engine(dir.path(), limits(1, 100), "exit 128\n");
        let (b, zero) = ("b".repeat(40), "0".repeat(40));
        let line = format!("{zero} {b} refs/notes/x");
        let create =
            vec![crate::secret_check::git::RefUpdate::parse_line(line.as_bytes()).unwrap()];
        let push = crate::git::access::PushContext::new(WORKSPACE, "bob").default_branch("main");
        crate::api::pushes::recheck(&engine, &db, &push, None, dir.path().join("repo"), create)
            .await;
        assert_eq!(
            relaxation_details(&db).await,
            [serde_json::json!({
                "ref": "refs/notes/x", "old": zero, "new": b, "rule": "ref_namespace", "override": null
            })]
        );
    }

    use crate::metrics::{Metrics, Snapshot, sample};

    fn git_count(m: &Metrics, service: &str, outcome: &str) -> f64 {
        let series =
            format!("potluck_git_requests_total{{service=\"{service}\",outcome=\"{outcome}\"}}");
        sample(&m.render(&Snapshot::default()), &series).unwrap_or(0.0)
    }

    #[tokio::test]
    async fn advertise_counts_ok_and_failed() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, repo) = engine_in(dir.path(), GitLimits::default()).await;
        let m = Metrics::new("t");
        let engine = engine.with_metrics(Arc::clone(&m));
        engine
            .advertise(Service::UploadPack, &repo, false)
            .await
            .unwrap();
        let _ = engine
            .advertise(Service::UploadPack, &dir.path().join("nope.git"), false)
            .await
            .unwrap_err();
        assert_eq!(git_count(&m, "advertise", "ok"), 1.0);
        assert_eq!(git_count(&m, "advertise", "failed"), 1.0);
        let text = m.render(&Snapshot::default());
        assert_eq!(
            sample(&text, "potluck_git_slot_wait_seconds_count"),
            Some(2.0),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_full_engine_counts_busy_and_the_wait() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, repo) = engine_in(dir.path(), limits(1, 100)).await;
        let m = Metrics::new("t");
        let engine = engine.with_metrics(Arc::clone(&m));
        let held = Arc::clone(&engine.permits).acquire_owned().await.unwrap();
        let err = engine
            .advertise(Service::UploadPack, &repo, false)
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::Busy), "got {err:?}");
        drop(held);
        assert_eq!(git_count(&m, "advertise", "busy"), 1.0);
        assert_eq!(
            git_count(&m, "advertise", "failed"),
            0.0,
            "busy is not also failed"
        );
        let text = m.render(&Snapshot::default());
        let le = "potluck_git_slot_wait_seconds_bucket{le=\"0.01\"}";
        assert_eq!(
            sample(&text, le),
            Some(0.0),
            "the 100 ms wait is above 10 ms: {text}"
        );
    }

    #[tokio::test]
    async fn a_git_past_its_run_limit_counts_overran() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake-git");
        write_script(&fake, "#!/bin/sh\nsleep 30\n", 0o755);
        std::fs::create_dir_all(dir.path().join("tmp")).unwrap();
        write_stub_hook(&dir.path().join("hooks"), 0o755);
        let m = Metrics::new("t");
        let engine = GitEngine::new(
            Repos::new(dir.path().join("repos"), fake),
            dir.path().join("hooks"),
            dir.path().join("tmp"),
            1024,
            limits(1, 100).with_max_runtime(Duration::from_secs(1)),
        )
        .with_metrics(Arc::clone(&m));
        let err = engine
            .advertise(
                Service::UploadPack,
                &dir.path().join("repos").join("x.git"),
                false,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::Overran { .. }), "got {err:?}");
        assert_eq!(git_count(&m, "advertise", "overran"), 1.0);
        assert_eq!(git_count(&m, "advertise", "failed"), 0.0);
    }

    #[tokio::test]
    async fn an_rpc_counts_once_when_its_reaper_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, repo) = engine_in(dir.path(), GitLimits::default()).await;
        let m = Metrics::new("t");
        let engine = engine.with_metrics(Arc::clone(&m));
        seed_commit(&repo, dir.path());
        let body = Body::from("0014command=ls-refs\n0000");
        let out = engine
            .rpc(Service::UploadPack, &repo, true, None, &[], body, None)
            .await
            .unwrap();
        let _ = collect(out).await;
        engine.tasks.close();
        engine.tasks.wait().await;
        assert_eq!(git_count(&m, "upload_pack", "ok"), 1.0);
        let too_big = Body::from(vec![b'x'; 2 * 1024 * 1024]);
        let _ = engine
            .rpc(Service::UploadPack, &repo, true, None, &[], too_big, None)
            .await
            .unwrap_err();
        assert_eq!(
            git_count(&m, "upload_pack", "failed"),
            1.0,
            "a refused body is failed"
        );
    }
}
