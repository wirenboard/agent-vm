//! agent-vm — sandboxed microVMs for AI coding agents on microsandbox.

mod clipboard;
mod defaults;
mod host_paths;
mod image_api_version;
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

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

// Shown under the top-level `agent-vm --help`, after the command list.
const TOP_AFTER_HELP: &str = "\
Getting started:
  agent-vm setup       fetch and verify the base image (run once first)
  cd ~/your-project
  agent-vm claude      launch in this project — or codex / opencode / copilot / shell

claude, codex, opencode, copilot and shell share the same options;
see `agent-vm claude --help` for mounts, ports, networking and credentials.";

#[derive(Parser)]
#[command(
    name = "agent-vm",
    version,
    about = "Sandboxed microVMs for AI coding agents.",
    after_help = TOP_AFTER_HELP
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pull and verify the base image (run once first).
    Setup(setup::Args),

    /// Refresh the cached base image.
    Pull(pull::Args),

    /// Launch Claude Code in a per-project sandbox.
    Claude(run::Args),

    /// Launch Codex CLI in a per-project sandbox.
    Codex(run::Args),

    /// Launch OpenCode in a per-project sandbox.
    Opencode(run::Args),

    /// Launch GitHub Copilot CLI in a per-project sandbox.
    Copilot(run::Args),

    /// Open a bash shell in a per-project sandbox.
    Shell(run::Args),

    /// Exchange a string between the host and the sandbox.
    Clipboard(clipboard::Args),

    /// Internal: invoked by msb's interceptor hook for matched OAuth
    /// refresh requests. Reads the request on stdin, writes an
    /// HTTP response on stdout. Not meant for direct use.
    #[command(name = "_intercept-hook", hide = true)]
    InterceptHook(intercept_hook::Args),
}

fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    // Locate and pin our patched msb binary via MSB_PATH so a user's
    // separate `~/.microsandbox/bin/msb` can't shadow ours. The hook
    // subcommand runs as a child of msb itself (the binary is
    // already resolved); the clipboard subcommand also runs in
    // contexts where the bundled msb may not be available
    // (e.g. inside the guest VM), so skip the check there too.
    //
    // CRITICAL: `point_at_msb()` / `point_at_msb_home()` mutate the
    // process environment via `unsafe { std::env::set_var(...) }`.
    // setenv() is not thread-safe under POSIX. We MUST run them
    // before the tokio multi-thread runtime spawns workers (which
    // happens inside `Runtime::new()`). Hence the manual sync `fn
    // main` + manual runtime construction instead of `#[tokio::main]`.
    let needs_msb_setup = !matches!(cli.cmd, Cmd::InterceptHook(_) | Cmd::Clipboard(_));
    if needs_msb_setup {
        msb_install::point_at_msb()?;
        // Reroute msb's writable state off `~/.microsandbox/` and into
        // agent-vm's own state dir. msb still finds `libkrunfw.so.*`
        // via MSB_PATH → sibling `../lib/` (the bundle layout), so no
        // copy/sync into MSB_HOME is needed — only the writable state
        // (db, sandboxes, cache, tls/CA, logs) lives here.
        msb_install::point_at_msb_home()?;
    }
    let runtime = tokio::runtime::Runtime::new().context("starting tokio runtime")?;
    runtime.block_on(async move {
        match cli.cmd {
            Cmd::Setup(args) => setup::run(args).await,
            Cmd::Pull(args) => pull::run(args).await,
            Cmd::Claude(args) => exit_with(run::launch(run::Agent::Claude, args).await?),
            Cmd::Codex(args) => exit_with(run::launch(run::Agent::Codex, args).await?),
            Cmd::Opencode(args) => exit_with(run::launch(run::Agent::Opencode, args).await?),
            Cmd::Copilot(args) => exit_with(run::launch(run::Agent::Copilot, args).await?),
            Cmd::Shell(args) => exit_with(run::launch(run::Agent::Shell, args).await?),
            Cmd::Clipboard(args) => clipboard::run(args),
            Cmd::InterceptHook(args) => intercept_hook::run(args).await,
        }
    })
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
