//! `potluck health-check`: asks the running server for its health over the admin socket.

use std::path::PathBuf;
use std::time::Duration;

/// The whole check, connect to last byte, must finish inside this; the image's
/// HEALTHCHECK kills the probe at 5 s (Containerfile), so this stays well below.
const HEALTH_BUDGET: Duration = Duration::from_secs(3);

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
    #[error("cannot reach the admin socket {path}")]
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
    /// The socket accepted but no complete answer arrived in time.
    #[error("no answer from the admin socket {path} within {} s", .budget.as_secs())]
    TimedOut {
        /// The socket that was tried.
        path: PathBuf,
        /// How long the check waited.
        budget: Duration,
    },
}

/// `GET /api/v1/health` over `<data>/admin.sock` within `HEALTH_BUDGET`; `Ok` only on HTTP 200.
pub async fn run(args: HealthArgs) -> Result<(), HealthError> {
    let path = args.data.join("admin.sock");
    let answer = tokio::time::timeout(
        HEALTH_BUDGET,
        crate::unix_http::get(&path, "/api/v1/health"),
    );
    match answer.await {
        Err(_) => Err(HealthError::TimedOut {
            path,
            budget: HEALTH_BUDGET,
        }),
        Ok(Ok((200, _))) => Ok(()),
        Ok(Ok((status, _))) => Err(HealthError::Unhealthy { status }),
        Ok(Err(source)) => Err(HealthError::Unreachable { path, source }),
    }
}
