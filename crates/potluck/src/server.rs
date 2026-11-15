//! The listeners: TCP for clients and the mode-0600 admin Unix socket for the local CLI.

use std::io;
use std::path::Path;

use axum::{Extension, Router};

use crate::api::Actor;

/// The router with `actor` attached to every request as an `Extension`.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "wired by the server start, which follows")
)]
pub(crate) fn with_actor(router: Router, actor: Actor) -> Router {
    router.layer(Extension(actor))
}

/// Binds `path` as a mode-0600 Unix socket, replacing a stale file or dead socket. A socket
/// that still answers means another potluck serves this data directory: `AddrInUse`.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "wired by the server start, which follows")
)]
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
