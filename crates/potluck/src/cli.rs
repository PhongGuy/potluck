//! The command line surface of the `potluck` binary.

use clap::{Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

use crate::config::{Config, ServeArgs};

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
}

/// Runs the parsed command to completion. `serve` returns after SIGTERM or Ctrl-C.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Serve(args) => serve(args).await,
    }
}

/// Validates the config before anything touches the disk or the log, then serves until a
/// stop signal and drains both listeners.
async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let config = Config::try_from(args)?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(config.log())?)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|err| anyhow::anyhow!(err))?;
    let running = crate::server::start(&config).await?;
    tracing::info!(
        "listening on http://{} ; admin socket {}",
        running.http_addr(),
        running.admin_socket().display()
    );
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = terminate.recv() => {}
        result = tokio::signal::ctrl_c() => result?,
    }
    running.shutdown().await;
    Ok(())
}
