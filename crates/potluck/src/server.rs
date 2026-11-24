//! The listeners: TCP for clients and the mode-0600 admin Unix socket for the local CLI.

use std::fs::{File, OpenOptions, TryLockError};
use std::future::IntoFuture;
use std::io;
use std::net::{Shutdown, SocketAddr};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

use axum::{Extension, Router};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;

use crate::api::{Actor, AppState};
use crate::config::{Config, ConfigError};
use crate::db::{Db, DbError};
use crate::notices::Notices;
use crate::update::{UpdateChecker, UpdateSource, schedule};

/// Why the server could not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServerError {
    /// A directory or the lock file under `--data` could not be created.
    #[error("cannot create {path}")]
    DataDir {
        /// The directory or lock file that could not be created.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
    /// Another potluck holds `<data>/server.lock`, so it is serving this data directory.
    #[error("the data directory {path} is in use by another potluck serve; stop it first")]
    DataDirInUse {
        /// The data directory from `--data`.
        path: PathBuf,
    },
    /// The filesystem refused the lock on `<data>/server.lock`, as NFS without lock support does.
    #[error(
        "cannot take the lock on {path}; keep the data directory on a local filesystem that supports file locks"
    )]
    DataDirLock {
        /// The lock file whose lock was refused.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
    /// The TCP listen address could not be bound.
    #[error("cannot listen on {addr}")]
    Listen {
        /// The address from `--listen`.
        addr: SocketAddr,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
    /// The admin socket could not be bound, including when another instance holds it.
    #[error("cannot bind the admin socket {path}")]
    AdminSocket {
        /// The socket path under `--data`.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
    /// The pre-receive hook could not be written.
    #[error("cannot write the pre-receive hook under {path}: {source}")]
    Hooks {
        /// The hooks directory under `--data`.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
    /// Opening or migrating `server.db` failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// The configuration cannot be served, such as an admin socket path too long to bind.
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// Refuses an admin socket path too long to bind before creating anything, then creates
/// `<data>`, `<data>/repos`, `<data>/hooks` and `<data>/tmp`, locks `<data>/server.lock`
/// for the life of the server, sweeps what killed pushes left in its repositories, installs the
/// pre-receive hook, opens and migrates the database, binds `config.listen()` and the admin
/// socket, and serves the API on both, plus `/metrics` on `config.metrics_listen()` when set.
/// The update endpoints report the check off.
pub async fn start(config: &Config) -> Result<Running, ServerError> {
    start_inner(config, Serve::Fresh(None)).await
}

/// As `start`, with an update checker over `source` for the running version; the daily check
/// runs unless `--no-update-check`. A checker that cannot be built is logged and left out.
pub async fn start_with_updates(
    config: &Config,
    source: UpdateSource,
) -> Result<Running, ServerError> {
    start_inner(config, Serve::Fresh(Some(source))).await
}

/// As `start`, serving `state` as given: its database (migrated here), resolver, limits,
/// hashing and update checker. `config` supplies the data dir, lock, hooks, listen address and
/// admin socket, and should be the config `state` was built from. No daily update loop runs.
pub async fn start_with(config: &Config, state: AppState) -> Result<Running, ServerError> {
    start_inner(config, Serve::Given(state)).await
}

/// What a start serves: a state built here from the config, optionally with an update checker
/// over a source, or a state the caller prepared.
enum Serve {
    Fresh(Option<UpdateSource>),
    Given(AppState),
}

async fn start_inner(config: &Config, serve: Serve) -> Result<Running, ServerError> {
    config.check_admin_socket()?;
    for path in [
        config.data().to_path_buf(),
        config.repos_dir(),
        config.hooks_dir(),
        config.tmp_dir(),
    ] {
        std::fs::create_dir_all(&path).map_err(|source| ServerError::DataDir { path, source })?;
    }
    // Lock before anything opens, writes or binds, so a second instance touches nothing.
    let _lock = lock_data_dir(config)?;
    // Nothing can push until the listeners bind, so leftovers of a receive-pack killed while no
    // server ran are swept here. A sweep that fails is logged and never stops the start.
    let repos = config.repos_dir();
    // Repositories it deferred to a live gc stay dirty, so the first push out sweeps them.
    let deferred = match tokio::task::spawn_blocking(move || crate::git::sweep_all(&repos)).await {
        Ok(swept) => {
            if swept.removed > 0 {
                tracing::info!(
                    removed = swept.removed,
                    "swept killed-push leftovers at startup"
                );
            }
            swept.deferred
        }
        Err(err) => {
            tracing::warn!(error = %err, "startup sweep task failed");
            Vec::new()
        }
    };
    let hooks_dir = config.hooks_dir();
    std::env::current_exe()
        .and_then(|exe| crate::hooks::install(&hooks_dir, &exe, config.git()))
        .map_err(|source| ServerError::Hooks {
            path: hooks_dir,
            source,
        })?;
    let (db, source, given) = match serve {
        Serve::Fresh(source) => {
            let db = Db::open(&config.db_path())?;
            db.migrate().await?;
            (db, source, None)
        }
        Serve::Given(state) => {
            state.db().migrate().await?;
            (state.db().clone(), None, Some(state))
        }
    };
    // Keep a duplicate of each listening socket so shutdown can take it out of LISTEN itself.
    let Bound {
        tcp,
        kept: kept_tcp,
        addr: http_addr,
    } = bind_tcp(config.listen())?;
    let metrics_tcp = config.metrics_listen().map(bind_tcp).transpose()?;
    let admin_socket = config.admin_socket();
    let unix = bind_admin_socket(&admin_socket).map_err(|source| ServerError::AdminSocket {
        path: admin_socket.clone(),
        source,
    })?;
    // The same duplicate for the admin socket, so a forked child's copy cannot keep it accepting.
    let kept_unix = unix
        .as_fd()
        .try_clone_to_owned()
        .map(std::os::unix::net::UnixListener::from)
        .map_err(|source| ServerError::AdminSocket {
            path: admin_socket.clone(),
            source,
        })?;

    // One stop flag shared by all three listeners, so shutdown drains them together.
    // TCP carries the peer address for `ClientIp`; the admin socket has none and stays local.
    let (stop, rx) = watch::channel(false);
    let mut state = given.unwrap_or_else(|| AppState::new(db.clone(), config.clone()));
    state.git().mark_dirty(&deferred);
    let notices = state.notices().clone();
    let git_tasks = state.git().tasks().clone();
    // The pruner and the daily loop ride the git tracker and the stop flag, so shutdown ends them
    // before their wait.
    crate::retention::spawn_pruner(db.clone(), config, &git_tasks, rx.clone());
    match source.map(|source| UpdateChecker::new(source, env!("CARGO_PKG_VERSION"))) {
        None => {}
        Some(Ok(checker)) => {
            if config.update_check() {
                schedule::spawn_daily(checker.clone(), &git_tasks, rx.clone());
            }
            state = state.with_updates(checker);
        }
        Some(Err(err)) => {
            tracing::error!("cannot set up the update check ({err}); serving without it");
        }
    }
    let metrics_router = crate::metrics::router(state.clone());
    let router = crate::api::router(state);
    let http = tokio::spawn(
        axum::serve(
            tcp,
            with_actor(router.clone(), Actor::Anonymous)
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(stopped(rx.clone()))
        .into_future(),
    );
    let admin = tokio::spawn(
        axum::serve(unix, with_actor(router, Actor::System))
            .with_graceful_shutdown(stopped(rx.clone()))
            .into_future(),
    );
    let mut tasks = vec![http, admin];
    let (metrics_addr, metrics_kept) = match metrics_tcp {
        Some(Bound { tcp, kept, addr }) => {
            tasks.push(tokio::spawn(
                axum::serve(tcp, metrics_router)
                    .with_graceful_shutdown(stopped(rx))
                    .into_future(),
            ));
            (Some(addr), Some(kept))
        }
        None => (None, None),
    };
    Ok(Running {
        http_addr,
        metrics_addr,
        admin_socket,
        stop,
        tasks,
        git_tasks,
        db,
        notices,
        tcp: kept_tcp,
        metrics_tcp: metrics_kept,
        unix: kept_unix,
        _lock,
    })
}

/// Takes the exclusive advisory lock on `config.lock_path()`. The returned `File` is the lock:
/// dropping it, or the process ending for any reason, releases it. The file itself stays.
/// Dropping it alone does not release the lock while a forked child still holds a duplicate
/// of its fd; `Running::shutdown` unlocks explicitly for that reason.
fn lock_data_dir(config: &Config) -> Result<File, ServerError> {
    let path = config.lock_path();
    // Never truncate: the file carries no content and another instance may be mid-open on it.
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|source| ServerError::DataDir { path, source })?;
    file.try_lock()
        .map(|()| file)
        .map_err(|err| lock_refused(err, config))
}

/// Why the lock was not taken: another holder is `DataDirInUse`, a filesystem that cannot
/// lock at all is `DataDirLock`.
fn lock_refused(err: TryLockError, config: &Config) -> ServerError {
    match err {
        TryLockError::WouldBlock => ServerError::DataDirInUse {
            path: config.data().to_path_buf(),
        },
        TryLockError::Error(source) => ServerError::DataDirLock {
            path: config.lock_path(),
            source,
        },
    }
}

/// A bound TCP listener, the duplicate kept for shutdown, and its resolved address.
struct Bound {
    tcp: tokio::net::TcpListener,
    kept: std::net::TcpListener,
    addr: SocketAddr,
}

/// Binds `addr` for tokio and keeps a duplicate of the socket, as the API listener does; any
/// failure is `ServerError::Listen` naming `addr`.
fn bind_tcp(addr: SocketAddr) -> Result<Bound, ServerError> {
    let listen = |source| ServerError::Listen { addr, source };
    let std_tcp = std::net::TcpListener::bind(addr).map_err(listen)?;
    std_tcp.set_nonblocking(true).map_err(listen)?;
    let kept = std_tcp.try_clone().map_err(listen)?;
    let tcp = tokio::net::TcpListener::from_std(std_tcp).map_err(listen)?;
    let addr = tcp.local_addr().map_err(listen)?;
    Ok(Bound { tcp, kept, addr })
}

/// Resolves once the stop flag changes or its sender is gone.
async fn stopped(mut rx: watch::Receiver<bool>) {
    let _ = rx.changed().await;
}

/// A started server. Dropping it without `shutdown` signals a graceful stop but neither
/// waits for in-flight requests nor removes the socket file.
#[non_exhaustive]
pub struct Running {
    http_addr: SocketAddr,
    metrics_addr: Option<SocketAddr>,
    admin_socket: PathBuf,
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<io::Result<()>>>,
    git_tasks: TaskTracker,
    db: Db,
    notices: Notices,
    tcp: std::net::TcpListener,
    metrics_tcp: Option<std::net::TcpListener>,
    unix: std::os::unix::net::UnixListener,
    _lock: File,
}

impl std::fmt::Debug for Running {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Running")
            .field("http_addr", &self.http_addr)
            .field("metrics_addr", &self.metrics_addr)
            .field("admin_socket", &self.admin_socket)
            .finish_non_exhaustive()
    }
}

impl Running {
    /// The bound TCP address, with the port resolved when `--listen` asked for 0.
    pub fn http_addr(&self) -> SocketAddr {
        self.http_addr
    }

    /// The bound metrics address, when `--metrics-listen` is set, with a port 0 resolved.
    pub fn metrics_addr(&self) -> Option<SocketAddr> {
        self.metrics_addr
    }

    /// The admin socket path.
    pub fn admin_socket(&self) -> &Path {
        &self.admin_socket
    }

    /// The database handle the server runs on; tests seed rows through it.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Stops accepting, waits for in-flight requests, takes every socket out of LISTEN, then waits
    /// for git reapers, push records and workspace creates and deletes, closes the database,
    /// removes the socket file, and releases the lock while leaving `server.lock` in place.
    /// Each git child stops at `--git-max-runtime-secs`, but a request or task runs its children
    /// in turn, so a hung push can hold shutdown for up to three limits (a ref listing,
    /// receive-pack, a ref listing) and a workspace create for one per git step. The port, the
    /// admin socket and the lock are free when it returns, even if a child forked meanwhile still
    /// holds duplicates of their fds.
    pub async fn shutdown(self) {
        // Open streams would hold the graceful drain forever; end them before stopping.
        self.notices.close_all();
        let _ = self.stop.send(true);
        for task in self.tasks {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => tracing::error!("listener failed: {err}"),
                Err(err) => tracing::error!("listener task failed: {err}"),
            }
        }
        // Closing our fds is not enough: shutdown(2) acts on the shared socket, so a forked
        // child's duplicate cannot keep it listening. It runs before the tracker wait so the
        // kernel refuses new connections instead of queueing them for a reset later.
        let tcp = std::net::TcpStream::from(OwnedFd::from(self.tcp));
        // On macOS and BSD shutdown(2) on a listening socket fails with ENOTCONN, so it only logs.
        if let Err(err) = tcp.shutdown(Shutdown::Both) {
            tracing::error!("cannot shut down the TCP listener: {err}");
        }
        if let Some(kept) = self.metrics_tcp {
            let metrics = std::net::TcpStream::from(OwnedFd::from(kept));
            if let Err(err) = metrics.shutdown(Shutdown::Both) {
                tracing::error!("cannot shut down the metrics listener: {err}");
            }
        }
        let unix = std::os::unix::net::UnixStream::from(OwnedFd::from(self.unix));
        if let Err(err) = unix.shutdown(Shutdown::Both) {
            tracing::error!("cannot shut down the admin socket listener: {err}");
        }
        // Reapers, push records and workspace creates and deletes need the database, so they
        // finish before it closes.
        self.git_tasks.close();
        self.git_tasks.wait().await;
        self.db.close();
        // The socket may already be gone; only its absence matters.
        let _ = std::fs::remove_file(&self.admin_socket);
        // LOCK_UN acts on the lock's open file description, so a forked child's duplicate
        // cannot keep the lock held.
        if let Err(err) = self._lock.unlock() {
            tracing::error!("cannot release the data directory lock: {err}");
        }
    }
}

/// The router with `actor` attached to every request as an `Extension`.
pub(crate) fn with_actor(router: Router, actor: Actor) -> Router {
    router.layer(Extension(actor))
}

/// Binds `path` as a mode-0600 Unix socket, replacing a stale file or dead socket. A socket
/// that still answers means another potluck serves this data directory: `AddrInUse`.
pub(crate) fn bind_admin_socket(path: &Path) -> io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    // A socket that still accepts a connection belongs to a live server on this data
    // directory; replacing it would split the two instances' views of one database.
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "another potluck is serving this data directory",
        ));
    }
    // Bind under a temporary name, tighten the mode, then rename over the final path: the
    // socket is never reachable at its public name with a looser mode, and a stale socket
    // from a crashed run is replaced atomically.
    let tmp = path.with_extension("sock.tmp");
    let _ = std::fs::remove_file(&tmp);
    let listener = std::os::unix::net::UnixListener::bind(&tmp)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    listener.set_nonblocking(true)?;
    tokio::net::UnixListener::from_std(listener)
}

#[cfg(test)]
mod tests {
    use std::future::IntoFuture;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::time::Duration;

    use axum::routing::get;
    use axum::{Extension, Router};

    use super::*;
    use crate::admin::client::{AdminClient, Verb};

    fn actor_route() -> Router {
        Router::new().route(
            "/actor",
            get(|Extension(actor): Extension<Actor>| async move { format!("{actor:?}") }),
        )
    }

    #[tokio::test]
    async fn requests_over_the_admin_socket_act_as_system() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("admin.sock");
        let listener = bind_admin_socket(&socket).unwrap();
        let task = tokio::spawn(
            axum::serve(listener, with_actor(actor_route(), Actor::System)).into_future(),
        );
        let response = AdminClient::connect(&socket)
            .await
            .unwrap()
            .send(Verb::Get, "/actor", None)
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            200,
            "the test route answers over the socket"
        );
        assert_eq!(
            response.text(),
            "System",
            "socket requests carry Actor::System"
        );
        task.abort();
    }

    #[tokio::test]
    async fn tcp_requests_act_as_anonymous() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(
            axum::serve(listener, with_actor(actor_route(), Actor::Anonymous)).into_future(),
        );
        let body = reqwest::get(format!("http://{addr}/actor"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "Anonymous", "TCP requests carry Actor::Anonymous");
        task.abort();
    }

    #[test]
    fn bind_sets_mode_0600_and_replaces_a_stale_file() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("admin.sock");
        std::fs::write(&socket, b"stale").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let _listener = bind_admin_socket(&socket).unwrap();
        let meta = std::fs::metadata(&socket).unwrap();
        assert!(
            meta.file_type().is_socket(),
            "the stale file became a socket"
        );
        assert_eq!(meta.permissions().mode() & 0o777, 0o600, "mode");
        assert!(
            !socket.with_extension("sock.tmp").exists(),
            "the temporary name is gone"
        );
    }

    #[tokio::test]
    async fn bind_refuses_a_live_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("admin.sock");
        let _first = bind_admin_socket(&socket).unwrap();
        let err = bind_admin_socket(&socket).expect_err("a live socket is not replaced");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "got {err:?}");
        assert!(
            err.to_string().contains("another potluck"),
            "the message says what is wrong: {err}"
        );
    }

    /// A config serving `dir` on an ephemeral loopback port, built the way `serve` builds one.
    fn config_for(dir: &Path) -> Config {
        config_with(dir, &[])
    }

    /// As `config_for`, with `extra` serve flags appended.
    fn config_with(dir: &Path, extra: &[&str]) -> Config {
        #[derive(clap::Parser)]
        struct Wrap {
            #[command(flatten)]
            serve: crate::config::ServeArgs,
        }
        let data = dir.to_str().expect("utf-8 temp path");
        let url = "http://127.0.0.1:8080";
        let argv = [
            "potluck",
            "--listen",
            "127.0.0.1:0",
            "--data",
            data,
            "--public-url",
            url,
        ];
        let argv = argv.iter().chain(extra);
        let wrap = <Wrap as clap::Parser>::try_parse_from(argv).expect("flags parse");
        Config::try_from(wrap.serve).expect("test config")
    }

    // The audit pruner is the one task always on the tracker right after start, so a count one
    // above it shows the flag spawned the daily loop. The GitHub source is safe: its first check
    // waits minutes.
    #[tokio::test]
    async fn the_daily_loop_is_spawned_only_when_the_check_is_on() {
        for (flags, loops) in [(&[][..], 2), (&["--no-update-check"][..], 1)] {
            let dir = tempfile::tempdir().unwrap();
            let source = UpdateSource::github().unwrap();
            let running = start_with_updates(&config_with(dir.path(), flags), source)
                .await
                .unwrap();
            assert_eq!(running.git_tasks.len(), loops, "{flags:?}");
            running.shutdown().await;
        }
    }

    #[tokio::test]
    async fn start_with_never_spawns_the_daily_loop() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(dir.path(), &[]);
        let source = UpdateSource::github().unwrap();
        let checker = UpdateChecker::new(source, env!("CARGO_PKG_VERSION")).unwrap();
        let db = Db::open(&config.db_path()).unwrap();
        let state = AppState::new(db, config.clone()).with_updates(checker);
        let running = start_with(&config, state).await.unwrap();
        assert_eq!(
            running.git_tasks.len(),
            1,
            "a given state gets the pruner and no daily loop"
        );
        running.shutdown().await;
    }

    // A second fd on the lock's open file description stands in for a child forked by another
    // thread and caught before exec: shutdown must release the lock regardless.
    #[tokio::test]
    async fn shutdown_releases_the_lock_while_a_duplicate_fd_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_for(dir.path());
        let running = start(&config).await.unwrap();
        let duplicate = running._lock.try_clone().unwrap();
        running.shutdown().await;
        let again = start(&config).await;
        drop(duplicate);
        let again = again.expect("the restart takes the lock");
        again.shutdown().await;
    }

    // The same stand-in for the listening socket: after shutdown nothing may accept on the port.
    #[tokio::test]
    async fn shutdown_closes_the_port_while_a_duplicate_fd_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let running = start(&config_for(dir.path())).await.unwrap();
        let addr = running.http_addr();
        let duplicate = running.tcp.try_clone().unwrap();
        running.shutdown().await;
        let connected = std::net::TcpStream::connect(addr);
        drop(duplicate);
        assert!(connected.is_err(), "the port still accepts after shutdown");
    }

    // The admin socket's path is gone after shutdown, so a hard link made beforehand is the only
    // way left to reach it: nothing may accept through it while a duplicate fd is open.
    #[tokio::test]
    async fn shutdown_closes_the_admin_socket_while_a_duplicate_fd_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let running = start(&config_for(dir.path())).await.unwrap();
        let alias = dir.path().join("alias.sock");
        std::fs::hard_link(running.admin_socket(), &alias).unwrap();
        let duplicate = running.unix.try_clone().unwrap();
        running.shutdown().await;
        let connected = std::os::unix::net::UnixStream::connect(&alias);
        drop(duplicate);
        assert!(
            connected.is_err(),
            "the admin socket still accepts after shutdown"
        );
    }

    // A task held on the tracker keeps shutdown waiting after the listeners drain; the port
    // must refuse new connections through that wait, not queue them for a reset later.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_port_refuses_connections_while_shutdown_waits_for_tracked_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let running = start(&config_for(dir.path())).await.unwrap();
        let addr = running.http_addr();
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        running.git_tasks.spawn(async move {
            let _ = held.await;
        });
        let shutdown = tokio::spawn(running.shutdown());
        // Each try that the kernel still accepts takes a backlog slot, so the tries stay well
        // under the listen backlog and a refusal is told apart from a full queue by its kind.
        let mut refused = false;
        for _ in 0..50 {
            match std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
                Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {
                    refused = true;
                    break;
                }
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        let still_waiting = !shutdown.is_finished();
        let _ = release.send(());
        shutdown.await.unwrap();
        assert!(
            still_waiting,
            "shutdown returned before the tracked task was released"
        );
        assert!(
            refused,
            "the port still queued connections while shutdown waited"
        );
    }

    #[test]
    fn a_filesystem_that_cannot_lock_is_reported_as_a_lock_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_for(dir.path());
        let nolck = std::io::Error::from(rustix::io::Errno::NOLCK);
        let err = lock_refused(std::fs::TryLockError::Error(nolck), &config);
        let ServerError::DataDirLock { path, .. } = &err else {
            panic!("got {err:?}");
        };
        assert_eq!(path, &config.lock_path());
        let text = err.to_string();
        assert!(
            text.contains(&format!(
                "cannot take the lock on {}",
                config.lock_path().display()
            )),
            "{text}"
        );
        assert!(
            text.contains("local filesystem that supports file locks"),
            "{text}"
        );
        assert!(!text.contains("cannot create"), "{text}");
        let busy = lock_refused(std::fs::TryLockError::WouldBlock, &config);
        assert!(
            matches!(busy, ServerError::DataDirInUse { .. }),
            "got {busy:?}"
        );
    }

    #[tokio::test]
    async fn connect_fails_when_the_socket_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let err = AdminClient::connect(&dir.path().join("admin.sock"))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "got {err:?}");
    }
}
