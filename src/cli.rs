use clap::{Parser, Subcommand};

use crate::{doctor, error::Result};

#[derive(Debug, Parser)]
#[command(
    name = "thrn",
    version,
    about = "A local-first developer CLI with read-only diagnostics."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report the CLI build metadata without changing files or using the network.
    Doctor,
}

pub fn run() -> Result<()> {
    match Cli::try_parse()
        .map_err(crate::error::ThrnError::from)?
        .command
    {
        Command::Doctor => doctor::run(),
    }
}
