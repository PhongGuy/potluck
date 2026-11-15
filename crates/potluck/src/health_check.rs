//! `potluck health-check`: asks the running server for its health over the admin socket.

use std::path::PathBuf;

/// Flags of `potluck health-check`.
#[derive(clap::Args, Debug, Clone)]
#[non_exhaustive]
pub struct HealthArgs {
    /// The running server's data directory, where `admin.sock` lives.
    #[arg(long, env = "POTLUCK_DATA", default_value = "./data")]
    data: PathBuf,
}

/// Why the check failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HealthError {
    /// No server answers on the socket (missing file, refused connection, bad response).
    #[error("cannot reach the admin socket {path}: {source}")]
    Unreachable {
        /// The socket that was tried.
        path: PathBuf,
        /// What went wrong while connecting or reading the response.
        #[source]
        source: std::io::Error,
    },
    /// The server answered, but not with 200.
    #[error("the health endpoint answered HTTP {status}")]
    Unhealthy {
        /// The status the server answered with.
        status: u16,
    },
}

/// `GET /api/v1/health` over `<data>/admin.sock`; `Ok` only on HTTP 200.
pub async fn run(args: HealthArgs) -> Result<(), HealthError> {
    let path = args.data.join("admin.sock");
    match crate::unix_http::get(&path, "/api/v1/health").await {
        Ok((200, _)) => Ok(()),
        Ok((status, _)) => Err(HealthError::Unhealthy { status }),
        Err(source) => Err(HealthError::Unreachable { path, source }),
    }
}
