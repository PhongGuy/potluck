//! The `potluck` binary.

use clap::Parser;
use potluck::cli::Cli;

fn main() {
    Cli::parse();
}
