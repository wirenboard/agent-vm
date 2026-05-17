//! agent-vm — sandboxed microVMs for AI coding agents on microsandbox.

mod pull_progress;
mod run;
mod session;
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

    /// Launch Claude Code in a sandbox mounted at /workspace.
    Claude(run::Args),

    /// Launch Codex CLI in a sandbox mounted at /workspace.
    Codex(run::Args),

    /// Launch OpenCode in a sandbox mounted at /workspace.
    Opencode(run::Args),

    /// Open a bash shell in a sandbox mounted at /workspace.
    Shell(run::Args),
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Setup(args) => setup::run(args).await,
        Cmd::Claude(args) => exit_with(run::launch(run::Agent::Claude, args).await?),
        Cmd::Codex(args) => exit_with(run::launch(run::Agent::Codex, args).await?),
        Cmd::Opencode(args) => exit_with(run::launch(run::Agent::Opencode, args).await?),
        Cmd::Shell(args) => exit_with(run::launch(run::Agent::Shell, args).await?),
    }
}

/// Wire `tracing` so `RUST_LOG=agent_vm=debug,microsandbox=info` works.
/// Default level is `warn` — keeps normal output clean, but anything from
/// the microsandbox stack surfaces when you ask for it.
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .compact()
        .init();
}

fn exit_with(code: i32) -> Result<()> {
    std::process::exit(code);
}
