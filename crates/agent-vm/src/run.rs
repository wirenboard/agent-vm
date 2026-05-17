//! `agent-vm <agent> [args...]` — boot a per-project sandbox and attach to
//! the chosen agent (or a shell).
//!
//! Phase 2 only knows about env-var auth (`ANTHROPIC_API_KEY`,
//! `OPENAI_API_KEY`); host-rooted refresh-able credentials land in
//! Phase 3/4.

use std::{
    env,
    io::IsTerminal as _,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use microsandbox::{Sandbox, sandbox::PullPolicy};

use crate::session::ProjectSession;

/// Paths that the guest will tmpfs-mount at boot, wiping anything our
/// `patch` builder baked into the rootfs underneath them. We refuse to mirror
/// a host project rooted here and fall back to `/workspace` instead.
const TMPFS_GUEST_PREFIXES: &[&str] = &["/tmp", "/run", "/dev/shm", "/var/run"];

fn guest_path_is_safe(project: &Path) -> bool {
    let s = match project.to_str() {
        Some(s) => s,
        None => return false,
    };
    !TMPFS_GUEST_PREFIXES
        .iter()
        .any(|p| s == *p || s.starts_with(&format!("{p}/")))
}

/// Absolute directories that must exist inside the guest rootfs before
/// microsandbox can mount the project at its host path. Returns paths from
/// shallowest to deepest, e.g. for `/home/boger/work/foo`:
/// `["/home", "/home/boger", "/home/boger/work", "/home/boger/work/foo"]`.
/// The leaf is included because microsandbox validates `workdir` against the
/// rootfs at create time, *before* the bind mount is materialized — without
/// the empty mount point dir it errors with "workdir does not exist in
/// guest". The bind mount then overlays this empty dir with the host's
/// project contents at boot.
fn mkdir_chain(project: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut acc = PathBuf::new();
    for c in project.components() {
        acc.push(c.as_os_str());
        let s = acc.to_string_lossy().to_string();
        if s != "/" && !s.is_empty() {
            out.push(s);
        }
    }
    out
}

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

    // Mount the host project at the *same* absolute path inside the guest so
    // that anything the agent emits (compiler errors, stack traces, git
    // output, file:line references) names a path that's interpretable on the
    // host. The agent-vm-state mount is internal and stays at a fixed path.
    //
    // Exception: paths under tmpfs mount points (typically /tmp, /run,
    // /dev/shm) can't be mirrored, because the guest tmpfs-mounts those at
    // boot — that wipes any mount point our `patch` builder baked into the
    // rootfs. Fall back to /workspace and tell the user once.
    //
    // microsandbox VMs also cap the number of virtio devices via libkrun's
    // IRQ pool; each bind mount is one device on top of the OCI rootfs's two
    // (EROFS lower + ext4 upper). We therefore bind a *single* host
    // directory for all per-agent state and either symlink the agent's
    // expected home (claude, opencode) or redirect via an env var (codex).
    // Codex needs the env-var path because its CLI binary lives under
    // /root/.codex/packages, which a symlink would shadow.
    let host_path = session
        .project_dir
        .to_str()
        .context("project path contains non-UTF-8 bytes; not supported")?;
    let project_guest_path = if guest_path_is_safe(&session.project_dir) {
        host_path.to_string()
    } else {
        eprintln!(
            "==> Project path {host_path} is under a tmpfs mount; mounting at /workspace instead"
        );
        "/workspace".to_string()
    };
    let mut patch_builder_steps = mkdir_chain(Path::new(&project_guest_path));
    // PullPolicy::Always: microsandbox's default IfMissing keys its layer
    // cache by reference, so re-running `agent-vm setup` (which rebuilds
    // and re-pushes to the same :latest tag) would otherwise boot from the
    // stale cached manifest. Always re-checks the manifest at the
    // registry; cached layers whose digests still match are reused, so
    // the cost is one round-trip + a few bytes when nothing changed.
    let mut builder = Sandbox::builder(&session.sandbox_name)
        .image(image.as_str())
        .registry(|r| r.insecure())
        .pull_policy(PullPolicy::Always)
        .cpus(cpus)
        .memory(memory_mib)
        .workdir(project_guest_path.clone())
        .volume(project_guest_path.clone(), |m| m.bind(&session.project_dir))
        .volume("/agent-vm-state", |m| m.bind(&session.state_dir))
        .patch(|mut p| {
            for parent in patch_builder_steps.drain(..) {
                p = p.mkdir(parent, None);
            }
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

    let profile = env::var("AGENT_VM_PROFILE").is_ok();
    eprintln!(
        "==> Booting sandbox from {image} ({memory_mib} MiB, {cpus} vCPU; first run pulls layers, otherwise ~3s)"
    );
    let t_create = Instant::now();
    let config = builder.build().await.context("preparing sandbox config")?;
    let (progress, task) = Sandbox::create_with_pull_progress(config);
    let render_task = tokio::spawn(crate::pull_progress::render(progress));
    let sandbox = task
        .await
        .context("create-with-pull-progress join")?
        .context("creating sandbox")?;
    render_task.await.ok();
    if profile {
        eprintln!("[profile] create: {:?}", t_create.elapsed());
    }

    let cmd = agent.command();
    let agent_args = args.agent_args;
    let t_run = Instant::now();
    let exit = if std::io::stdin().is_terminal() {
        eprintln!("==> Attaching to {cmd}");
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
            .exec_with(cmd, |e| {
                e.args(agent_args).cwd(project_guest_path.clone())
            })
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

    if profile {
        eprintln!("[profile] run:    {:?}", t_run.elapsed());
    }

    eprintln!("==> Stopping sandbox");
    let t_stop = Instant::now();
    sandbox.stop_and_wait().await.ok();
    if profile {
        eprintln!("[profile] stop:   {:?}", t_stop.elapsed());
    }
    let t_remove = Instant::now();
    Sandbox::remove(&session.sandbox_name).await.ok();
    if profile {
        eprintln!("[profile] remove: {:?}", t_remove.elapsed());
    }

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
