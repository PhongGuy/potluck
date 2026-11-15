//! The listeners: TCP for clients and the mode-0600 admin Unix socket for the local CLI.

use std::future::IntoFuture;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::{Extension, Router};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::api::{Actor, AppState};
use crate::config::Config;
use crate::db::{Db, DbError};

/// Why the server could not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServerError {
    /// A directory under `--data` could not be created.
    #[error("cannot create {path}")]
    DataDir {
        /// The directory that could not be created.
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
}

/// Creates `<data>`, `<data>/repos`, `<data>/hooks` and `<data>/tmp`, installs the pre-receive
/// hook, opens and migrates the database, binds `config.listen()` and the admin socket, and
/// serves the API on both.
pub async fn start(config: &Config) -> Result<Running, ServerError> {
    for path in [
        config.data().to_path_buf(),
        config.repos_dir(),
        config.hooks_dir(),
        config.tmp_dir(),
    ] {
        std::fs::create_dir_all(&path).map_err(|source| ServerError::DataDir { path, source })?;
    }
    let hooks_dir = config.hooks_dir();
    crate::hooks::install(&hooks_dir).map_err(|source| ServerError::Hooks {
        path: hooks_dir,
        source,
    })?;
    let db = Db::open(&config.db_path())?;
    db.migrate().await?;
    let addr = config.listen();
    let tcp = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|source| ServerError::Listen { addr, source })?;
    let http_addr = tcp
        .local_addr()
        .map_err(|source| ServerError::Listen { addr, source })?;
    let admin_socket = config.admin_socket();
    let unix = bind_admin_socket(&admin_socket).map_err(|source| ServerError::AdminSocket {
        path: admin_socket.clone(),
        source,
    })?;

    // One stop flag shared by both listeners, so shutdown drains TCP and the socket together.
    let router = crate::api::router(AppState::new(db.clone(), config.clone()));
    let (stop, rx) = watch::channel(false);
    let http = tokio::spawn(
        axum::serve(tcp, with_actor(router.clone(), Actor::Anonymous))
            .with_graceful_shutdown(stopped(rx.clone()))
            .into_future(),
    );
    let admin = tokio::spawn(
        axum::serve(unix, with_actor(router, Actor::System))
            .with_graceful_shutdown(stopped(rx))
            .into_future(),
    );
    Ok(Running {
        http_addr,
        admin_socket,
        stop,
        tasks: [http, admin],
        db,
    })
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
    admin_socket: PathBuf,
    stop: watch::Sender<bool>,
    tasks: [JoinHandle<io::Result<()>>; 2],
    db: Db,
}

impl std::fmt::Debug for Running {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Running")
            .field("http_addr", &self.http_addr)
            .field("admin_socket", &self.admin_socket)
            .finish_non_exhaustive()
    }
}

impl Running {
    /// The bound TCP address, with the port resolved when `--listen` asked for 0.
    pub fn http_addr(&self) -> SocketAddr {
        self.http_addr
    }

    /// The admin socket path.
    pub fn admin_socket(&self) -> &Path {
        &self.admin_socket
    }

    /// Stops accepting, waits for in-flight requests, closes the database, removes the socket file.
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        for task in self.tasks {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => tracing::error!("listener failed: {err}"),
                Err(err) => tracing::error!("listener task failed: {err}"),
            }
        }
        self.db.close();
        // The socket may already be gone; only its absence matters.
        let _ = std::fs::remove_file(&self.admin_socket);
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

    use axum::routing::get;
    use axum::{Extension, Router};

    use super::*;

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
        let (status, body) = crate::unix_http::get(&socket, "/actor").await.unwrap();
        assert_eq!(status, 200, "the test route answers over the socket");
        assert_eq!(body, "System", "socket requests carry Actor::System");
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

    #[tokio::test]
    async fn get_fails_when_the_socket_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let err = crate::unix_http::get(&dir.path().join("admin.sock"), "/api/v1/health")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "got {err:?}");
    }
}
