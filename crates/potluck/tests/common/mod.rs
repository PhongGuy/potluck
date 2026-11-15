//! Shared helpers. Every test reaches only 127.0.0.1 or a Unix socket under a temp dir.

use std::process::Command;

/// The built `potluck` binary with an empty environment, so no `POTLUCK_*` variable or
/// real config leaks in. Tests add what they need with `.env(..)`.
pub fn potluck() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_potluck"));
    cmd.env_clear();
    cmd
}

use clap::Parser;
use potluck::config::{Config, ConfigError, ServeArgs};

#[derive(Parser)]
struct ServeWrap {
    #[command(flatten)]
    serve: ServeArgs,
}

/// Builds a `Config` from `potluck serve` flags. Pass every flag explicitly: clap also reads
/// `POTLUCK_*` from the real environment, and these tests must not depend on it.
pub fn parse_config(args: &[&str]) -> Result<Config, ConfigError> {
    let argv = std::iter::once("potluck").chain(args.iter().copied());
    let wrap = ServeWrap::try_parse_from(argv).expect("flags parse");
    Config::try_from(wrap.serve)
}
