//! agent-vm — sandboxed microVMs for AI coding agents on microsandbox.

mod setup;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agent-vm", about = "Sandboxed VMs for AI coding agents.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build (and verify) the agent-vm base image.
    Setup(setup::Args),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Setup(args) => setup::run(args).await,
    }
}
