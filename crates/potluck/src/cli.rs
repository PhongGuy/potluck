//! The command line surface of the `potluck` binary.

use std::io::IsTerminal;

use clap::{Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

use crate::config::{Config, ServeArgs};
use crate::health_check::HealthArgs;

/// The `potluck` command line.
#[derive(Parser, Debug)]
#[command(
    name = "potluck",
    version,
    about = "Self-hosted team server for local-first desktop apps"
)]
#[non_exhaustive]
pub struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The `potluck` subcommands.
#[derive(Subcommand, Debug)]
#[non_exhaustive]
pub enum Command {
    /// Run the server.
    Serve(ServeArgs),
    /// Exit 0 if the server on this data directory answers its health check.
    HealthCheck(HealthArgs),
}

/// Runs the parsed command to completion. `serve` returns after SIGTERM or Ctrl-C;
/// `health-check` prints `ok` when the server is healthy.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Serve(args) => serve(args).await,
        Command::HealthCheck(args) => {
            crate::health_check::run(args).await?;
            println!("ok");
            Ok(())
        }
    }
}

/// Validates the config before anything touches the disk or the log, then serves until a
/// stop signal and drains both listeners.
async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let config = Config::try_from(args)?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(config.log())?)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .try_init()
        .map_err(|err| anyhow::anyhow!(err))?;
    // Both handlers go in before start, so a SIGTERM or Ctrl-C during startup still reaches
    // shutdown; `ctrl_c()` would only register once polled.
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let running = crate::server::start(&config).await?;
    tracing::info!(
        "listening on http://{} ; admin socket {}",
        running.http_addr(),
        running.admin_socket().display()
    );
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
    running.shutdown().await;
    Ok(())
}
