//! The command line surface of the `potluck` binary.

use clap::Parser;

/// The `potluck` command line.
#[derive(Parser, Debug)]
#[command(
    name = "potluck",
    version,
    about = "Self-hosted team server for local-first desktop apps"
)]
#[non_exhaustive]
pub struct Cli {}
