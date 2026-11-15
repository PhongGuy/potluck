//! The command line surface of the `potluck` binary.

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

use crate::admin::AdminArgs;
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
    // Boxed: the serve flags would otherwise size every variant.
    /// Run the server.
    Serve(Box<ServeArgs>),
    /// Exit 0 if the server on this data directory answers its health check within 3 s.
    HealthCheck(HealthArgs),
    /// Manage users, tokens, teams, workspaces, grants and overrides over the admin socket.
    Admin(Box<AdminArgs>),
    /// Run by git from `<data>/hooks/pre-receive`; not for people.
    #[command(hide = true)]
    Hook(HookArgs),
}

/// Which hook to run.
#[derive(clap::Args, Debug)]
#[non_exhaustive]
pub struct HookArgs {
    /// The hook.
    #[command(subcommand)]
    pub hook: HookKind,
}

/// The hooks git may run.
#[derive(Subcommand, Debug)]
#[non_exhaustive]
pub enum HookKind {
    /// The secret check: `<old> <new> <ref>` lines on stdin, findings on stderr, exit 1 to refuse.
    PreReceive,
}

/// Runs the parsed command to completion. `serve` returns after SIGTERM or Ctrl-C;
/// `health-check` prints `ok` when the server is healthy; `admin` prints its verb's answer;
/// all give `ExitCode::SUCCESS` on `Ok`. `hook pre-receive` returns the hook's exit code, so
/// a refusal prints only its lines.
pub async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Serve(args) => serve(*args).await.map(|()| ExitCode::SUCCESS),
        Command::HealthCheck(args) => {
            crate::health_check::run(args).await?;
            println!("ok");
            Ok(ExitCode::SUCCESS)
        }
        Command::Admin(args) => {
            let mut stdin = std::io::stdin().lock();
            let mut stdout = std::io::stdout().lock();
            crate::admin::run(*args, &mut stdin, &mut stdout).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Hook(args) => Ok(match args.hook {
            HookKind::PreReceive => crate::secret_check::run_pre_receive(),
        }),
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
