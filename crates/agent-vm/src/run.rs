//! `agent-vm <agent> [args...]` — boot a per-project sandbox and attach to
//! the chosen agent (or a shell).
//!
//! Phase 2 only knows about env-var auth (`ANTHROPIC_API_KEY`,
//! `OPENAI_API_KEY`); host-rooted refresh-able credentials land in
//! Phase 3/4.

use std::{env, io::IsTerminal as _};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use microsandbox::Sandbox;

use crate::session::ProjectSession;

/// Which entry point to attach inside the sandbox.
#[derive(Clone, Copy)]
pub enum Agent {
    Claude,
    Codex,
    Opencode,
    Shell,
}

impl Agent {
    fn command(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Opencode => "opencode",
            Agent::Shell => "bash",
        }
    }
}

#[derive(ClapArgs)]
pub struct Args {
    /// Args forwarded verbatim to the in-sandbox agent command. Use `--` if
    /// any argument starts with `-` to keep clap from claiming it.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    agent_args: Vec<String>,
}

pub async fn launch(agent: Agent, args: Args) -> Result<i32> {
    let session = ProjectSession::for_cwd()?;
    session.ensure_dirs()?;
    eprintln!(
        "==> {} in {} (state: {})",
        session.sandbox_name,
        session.project_dir.display(),
        session.state_dir.display(),
    );
    let _ = &session.project_hash;

    let image = env::var("AGENT_VM_IMAGE_TAG")
        .unwrap_or_else(|_| "localhost:5000/agent-vm:latest".to_string());
    let memory_mib: u32 = parse_env_u32("AGENT_VM_MEMORY_MIB", 2048)?;
    let cpus_u32: u32 = parse_env_u32("AGENT_VM_CPUS", 2)?;
    let cpus: u8 = cpus_u32
        .try_into()
        .with_context(|| format!("AGENT_VM_CPUS={cpus_u32} exceeds u8::MAX"))?;

    // microsandbox VMs cap the number of virtio devices via libkrun's IRQ
    // pool; each bind mount is one device on top of the OCI rootfs's two
    // (EROFS lower + ext4 upper). We therefore bind a single host directory
    // for all per-agent state and either symlink the agent's expected home
    // (claude, opencode) or redirect via an env var (codex). Codex needs the
    // env-var path because its CLI binary lives under /root/.codex/packages,
    // which a symlink would shadow.
    let mut builder = Sandbox::builder(&session.sandbox_name)
        .image(image.as_str())
        .registry(|r| r.insecure())
        .cpus(cpus)
        .memory(memory_mib)
        .workdir("/workspace")
        .volume("/workspace", |m| m.bind(&session.project_dir))
        .volume("/agent-vm-state", |m| m.bind(&session.state_dir))
        .patch(|p| {
            p.mkdir("/root/.local", None)
                .mkdir("/root/.local/share", None)
                .symlink("/agent-vm-state/claude", "/root/.claude", true)
                .symlink(
                    "/agent-vm-state/opencode",
                    "/root/.local/share/opencode",
                    true,
                )
        })
        .env("CODEX_HOME", "/agent-vm-state/codex")
        .replace();

    // Phase 2: pass API keys straight through. Phase 3 replaces this with
    // host-rooted placeholder secrets.
    for var in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
        if let Ok(val) = env::var(var) {
            if !val.is_empty() {
                builder = builder.env(var, val);
            }
        }
    }
    // The image's PATH was set inside the Dockerfile, but it lives in the
    // shell rc files of /root. attach() launches the agent directly via
    // execve, so re-publish the same PATH here.
    builder = builder.env(
        "PATH",
        "/root/.local/bin:/root/.claude/local/bin:/root/.opencode/bin:/usr/local/bin:/usr/bin:/bin",
    );

    eprintln!(
        "==> Booting sandbox from {image} ({memory_mib} MiB, {cpus} vCPU; first run pulls layers, otherwise ~3s)"
    );
    let sandbox = builder
        .create()
        .await
        .context("creating sandbox")?;

    let cmd = agent.command();
    let agent_args = args.agent_args;
    let exit = if std::io::stdin().is_terminal() {
        eprintln!("==> Attaching to {cmd} (Ctrl-P Ctrl-Q to detach)");
        sandbox
            .attach(cmd, agent_args)
            .await
            .with_context(|| format!("attaching to {cmd}"))?
    } else {
        // No host TTY (piped, redirected, smoke-tested under `sg`/`sudo` etc.).
        // attach() needs a real /dev/tty for raw-mode stdin, so fall back to
        // collected exec. Output is forwarded once the command exits.
        eprintln!("==> Running {cmd} in sandbox (no TTY; output appears after exit)");
        use tokio::io::AsyncWriteExt as _;
        let output = sandbox
            .exec_with(cmd, |e| e.args(agent_args).cwd("/workspace"))
            .await
            .with_context(|| format!("running {cmd} in sandbox"))?;
        let mut stdout = tokio::io::stdout();
        stdout.write_all(output.stdout_bytes()).await.ok();
        stdout.flush().await.ok();
        let mut stderr = tokio::io::stderr();
        stderr.write_all(output.stderr_bytes()).await.ok();
        stderr.flush().await.ok();
        output.status().code
    };

    eprintln!("==> Stopping sandbox");
    sandbox.stop_and_wait().await.ok();
    Sandbox::remove(&session.sandbox_name).await.ok();

    Ok(exit)
}

fn parse_env_u32(name: &str, default: u32) -> Result<u32> {
    match env::var(name) {
        Ok(s) => s
            .parse()
            .with_context(|| format!("{name} must be a positive integer, got {s:?}")),
        Err(_) => Ok(default),
    }
    .and_then(|v| {
        if v == 0 {
            bail!("{name} must be > 0");
        }
        Ok(v)
    })
}
