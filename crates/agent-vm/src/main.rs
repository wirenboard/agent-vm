//! agent-vm — sandboxed microVMs for AI coding agents on microsandbox.

mod clipboard;
mod host_paths;
mod image_check;
mod intercept_hook;
mod msb_install;
mod pull;
mod pull_progress;
mod pulled_marker;
mod run;
mod secrets;
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

    /// Pull the latest image from the registry into the microsandbox cache.
    Pull(pull::Args),

    /// Launch Claude Code in a sandbox mounted at the project's host path.
    Claude(run::Args),

    /// Launch Codex CLI in a sandbox mounted at the project's host path.
    Codex(run::Args),

    /// Launch OpenCode in a sandbox mounted at the project's host path.
    Opencode(run::Args),

    /// Open a bash shell in a sandbox mounted at the project's host path.
    Shell(run::Args),

    /// Exchange a string between the host and the per-project sandbox.
    /// See `agent-vm clipboard --help`.
    Clipboard(clipboard::Args),

    /// Internal: invoked by msb's interceptor hook for matched OAuth
    /// refresh requests. Reads the request on stdin, writes an
    /// HTTP response on stdout. Not meant for direct use.
    #[command(name = "_intercept-hook", hide = true)]
    InterceptHook(intercept_hook::Args),
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    // Phase 4: prefer our locally-built msb that knows about
    // SecretValue::File and the request-interceptor hook. No-op if
    // the binary hasn't been built yet (run `agent-vm setup` to
    // build it).
    msb_install::point_at_workspace_msb();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Setup(args) => setup::run(args).await,
        Cmd::Pull(args) => pull::run(args).await,
        Cmd::Claude(args) => exit_with(run::launch(run::Agent::Claude, args).await?),
        Cmd::Codex(args) => exit_with(run::launch(run::Agent::Codex, args).await?),
        Cmd::Opencode(args) => exit_with(run::launch(run::Agent::Opencode, args).await?),
        Cmd::Shell(args) => exit_with(run::launch(run::Agent::Shell, args).await?),
        Cmd::Clipboard(args) => clipboard::run(args),
        Cmd::InterceptHook(args) => intercept_hook::run(args).await,
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
