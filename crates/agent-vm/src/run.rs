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

use anyhow::{Context, Result};
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

    /// Flags we always pass before the user's own args. The microVM is
    /// the security boundary, so the in-VM agent's "are you sure?"
    /// prompts add no protection and break agent-mode flows.
    fn default_args(self) -> &'static [&'static str] {
        match self {
            Agent::Claude => &["--dangerously-skip-permissions"],
            Agent::Codex | Agent::Opencode | Agent::Shell => &[],
        }
    }
}

#[derive(ClapArgs)]
pub struct Args {
    /// Sandbox memory in GiB.
    #[arg(long, env = "AGENT_VM_MEMORY_GIB", default_value_t = 2)]
    memory: u32,

    /// vCPU count for the sandbox.
    #[arg(long, env = "AGENT_VM_CPUS", default_value_t = 2)]
    cpus: u8,

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
    let memory_mib: u32 = args
        .memory
        .checked_mul(1024)
        .context("--memory in GiB overflows u32 MiB")?;
    let cpus = args.cpus;

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
    // PullPolicy::IfMissing keeps the slow part (pull + materialize) off
    // every launch. We separately HEAD the manifest at the registry via
    // image_check::check_for_update and print a banner if there's a newer
    // image available — the user runs `agent-vm pull` explicitly to
    // fetch it.
    notify_if_update_available(image.as_str()).await;

    // Snapshot host credentials into per-project token files and place
    // placeholder credentials.json files where the in-VM agents will
    // find them. The token files are passed to microsandbox below as
    // SecretValue::File entries; the proxy re-reads them on every
    // connection setup, so a host-side rotation propagates without
    // restarting the sandbox.
    //
    // Passing the in-guest project path lets us pre-approve it in
    // Claude's per-folder trust list (~/.claude.json `projects.<path>.
    // hasTrustDialogAccepted = true`), suppressing the "do you trust
    // this folder?" wizard on first launch in each project.
    let creds = crate::secrets::refresh(&session.state_dir, &project_guest_path)
        .context("snapshotting host credentials")?;

    let mut builder = Sandbox::builder(&session.sandbox_name)
        .image(image.as_str())
        .registry(|r| r.insecure())
        .pull_policy(PullPolicy::IfMissing)
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
                // Onboarding-state file lives at $HOME root, not in
                // .claude/. Without persistence the in-VM Claude
                // re-runs the theme picker every launch.
                .symlink("/agent-vm-state/claude.json", "/root/.claude.json", true)
                .symlink(
                    "/agent-vm-state/opencode",
                    "/root/.local/share/opencode",
                    true,
                )
        })
        .env("CODEX_HOME", "/agent-vm-state/codex")
        .replace();

    // For each provider with a host credential file, register a
    // SecretValue::File secret keyed on the placeholder string the
    // guest will send, then register the OAuth refresh endpoint as a
    // hook target so a 401-then-refresh attempt round-trips through a
    // real host-side rotation (see `intercept_hook`).
    //
    // allow_host covers both the API endpoint and the OAuth endpoint;
    // the OAuth host has to be allowed for the placeholder to leave
    // the VM at all (microsandbox's violation detector would block it
    // otherwise), even though substitution there is a no-op because
    // the body's refresh_token is a placeholder, not a header.
    if creds.anthropic_token_file.is_some() || creds.openai_token_file.is_some() {
        use crate::secrets::*;
        let anthropic = creds.anthropic_token_file.clone();
        let openai = creds.openai_token_file.clone();
        let opencode = creds.opencode_openai_access_token_file.clone();
        let self_path = std::env::current_exe().context("std::env::current_exe")?;
        let state_dir = session.state_dir.clone();
        builder = builder.network(move |mut n| {
            n = n.tls(|t| t);
            // We only ever substitute into Authorization: Bearer headers.
            // Explicitly disable basic_auth so the proxy's per-chunk fast
            // path can short-circuit when the placeholder isn't present
            // — critical for post-WebSocket-upgrade binary frames where
            // a UTF-8 lossy round trip would corrupt the bytes.
            if let Some(file) = anthropic {
                n = n.secret(|s| {
                    s.env("MSB_AGENT_VM_ANTHROPIC_UNUSED")
                        .value(file)
                        .placeholder(ANTHROPIC_ACCESS_PLACEHOLDER)
                        .inject_basic_auth(false)
                        .allow_host(ANTHROPIC_API_HOST)
                        .allow_host(ANTHROPIC_OAUTH_HOST)
                });
            }
            if let Some(file) = openai {
                n = n.secret(|s| {
                    s.env("MSB_AGENT_VM_OPENAI_UNUSED")
                        .value(file)
                        .placeholder(OPENAI_ACCESS_PLACEHOLDER)
                        .inject_basic_auth(false)
                        .allow_host(OPENAI_API_HOST)
                        .allow_host(OPENAI_CHATGPT_HOST)
                        .allow_host(OPENAI_OAUTH_HOST)
                });
            }
            // OpenCode sends Authorization: Bearer <synthetic JWT> to
            // api.openai.com; the proxy swaps the JWT for the real
            // OpenAI access token (same on-disk file as Codex uses).
            if let Some(file) = opencode {
                n = n.secret(|s| {
                    s.env("MSB_AGENT_VM_OPENCODE_OPENAI_UNUSED")
                        .value(file)
                        .placeholder(OPENCODE_OPENAI_ACCESS_PLACEHOLDER)
                        .inject_basic_auth(false)
                        .allow_host(OPENAI_API_HOST)
                        .allow_host(OPENAI_CHATGPT_HOST)
                });
            }
            n.intercept(|i| {
                i.hook([
                    self_path.to_string_lossy().to_string(),
                    "_intercept-hook".to_string(),
                    "--state-dir".to_string(),
                    state_dir.to_string_lossy().to_string(),
                ])
                .rule(ANTHROPIC_OAUTH_HOST, "POST", ANTHROPIC_OAUTH_TOKEN_PATH)
                .rule(OPENAI_OAUTH_HOST, "POST", OPENAI_OAUTH_TOKEN_PATH)
            })
        });
    }

    // Still honour ANTHROPIC_API_KEY / OPENAI_API_KEY if explicitly set
    // by the user — that path stays a simple Bearer header, no
    // placeholder substitution involved.
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

    // Claude Code refuses to run as root with --dangerously-skip-permissions
    // unless this env var is set. The whole point of running it in a
    // microVM is that the sandbox IS our security boundary, so the
    // in-guest CLI's extra guard would just block us from getting work
    // done. Same env var the original Bash agent-vm used.
    builder = builder.env("IS_SANDBOX", "1");

    let profile = env::var("AGENT_VM_PROFILE").is_ok();
    eprintln!(
        "==> Booting sandbox from {image} ({memory_mib} MiB, {cpus} vCPU; first run pulls layers, otherwise ~3s)"
    );
    let t_create = Instant::now();
    let config = builder.build().await.context("preparing sandbox config")?;
    if env::var("AGENT_VM_DEBUG_CONFIG").is_ok() {
        eprintln!(
            "[debug] sandbox config JSON: {}",
            serde_json::to_string_pretty(&config).unwrap_or_default()
        );
    }
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

    let inner_cmd = agent.command();
    // Prepend agent-vm's default flags (e.g. --dangerously-skip-permissions
    // for Claude) unless the user already provided them.
    let mut inner_args: Vec<String> = agent
        .default_args()
        .iter()
        .filter(|d| !args.agent_args.iter().any(|u| u == *d))
        .map(|s| s.to_string())
        .collect();
    inner_args.extend(args.agent_args);

    // Wrap the agent invocation in a tiny bash prelude that:
    //
    // 1. Strips IPv6 nameservers from /etc/resolv.conf before exec'ing
    //    the agent. microsandbox's agentd writes both v4 and v6 gateway
    //    DNS into the guest's /etc/resolv.conf at boot. The v6 entry was
    //    observed unresponsive in at least one nested-libkrun setup
    //    (gateway times out on v6 DNS queries), and codex's Rust async
    //    resolver returns EAI_AGAIN ("Try again") in that case instead
    //    of falling through to the working v4 resolver the way glibc's
    //    getaddrinfo does. Result: codex hangs at startup with "failed
    //    to lookup address information" for chatgpt.com, even though
    //    `getent hosts chatgpt.com` returns immediately. Stripping the
    //    v6 nameserver line makes the resolver single-stack, which is
    //    fine for outbound traffic to public APIs. The regex matches
    //    lines whose nameserver value contains a colon — IPv4 addresses
    //    never do, IPv6 addresses always do.
    //
    // 2. Redirects stdin to /dev/null when not on a TTY. exec_with's
    //    default `StdinMode::Null` was observed *not* to satisfy codex
    //    0.133's `exec` subcommand: codex blocks indefinitely on what it
    //    thinks is unbounded interactive input. Backgrounding codex (`&`)
    //    fixed it (bash auto-redirects stdin to /dev/null for background
    //    jobs) but we can't background the user's agent. An explicit
    //    `exec < /dev/null` gives codex a real /dev/null fd and it
    //    proceeds. `[ -t 0 ]` keeps interactive TTY launches unaffected.
    let prelude = r#"sed -i '/^nameserver .*:/d' /etc/resolv.conf 2>/dev/null || true
[ -t 0 ] || exec < /dev/null"#;
    let mut shell_line = String::from(prelude);
    shell_line.push_str("; exec ");
    shell_line.push_str(&shell_escape(inner_cmd));
    for a in &inner_args {
        shell_line.push(' ');
        shell_line.push_str(&shell_escape(a));
    }
    let cmd = "bash";
    let agent_args: Vec<String> = vec!["-c".into(), shell_line];

    let t_run = Instant::now();
    let exit = if std::io::stdin().is_terminal() {
        eprintln!("==> Attaching to {inner_cmd}");
        sandbox
            .attach(cmd, agent_args)
            .await
            .with_context(|| format!("attaching to {inner_cmd}"))?
    } else {
        // No host TTY (piped, redirected, smoke-tested under `sg`/`sudo` etc.).
        // attach() needs a real /dev/tty for raw-mode stdin, so use the
        // streaming exec API instead: write stdout/stderr to ours as they
        // arrive. That keeps progress visible on long-running agent
        // commands (codex exec can take >30s for a single response) and
        // lets us inspect partial output when the user Ctrl-Cs or the
        // shell times out.
        eprintln!("==> Running {inner_cmd} in sandbox (no TTY; streaming output)");
        use microsandbox::sandbox::exec::ExecEvent;
        use tokio::io::AsyncWriteExt as _;
        let mut handle = sandbox
            .exec_stream_with(cmd, |e| {
                e.args(agent_args).cwd(project_guest_path.clone())
            })
            .await
            .with_context(|| format!("running {inner_cmd} in sandbox"))?;
        let mut stdout = tokio::io::stdout();
        let mut stderr = tokio::io::stderr();
        let mut code: i32 = 1;
        while let Some(event) = handle.recv().await {
            match event {
                ExecEvent::Stdout(b) => {
                    stdout.write_all(&b).await.ok();
                    stdout.flush().await.ok();
                }
                ExecEvent::Stderr(b) => {
                    stderr.write_all(&b).await.ok();
                    stderr.flush().await.ok();
                }
                ExecEvent::Exited { code: c } => {
                    code = c;
                    break;
                }
                ExecEvent::Failed(payload) => {
                    anyhow::bail!("exec session failed: {payload:?}");
                }
                ExecEvent::Started { .. } | ExecEvent::StdinError(_) => {}
            }
        }
        code
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

    // Phase 5 safety net: diff the host credential files against the
    // SHA-256 snapshot we took at launch. The Phase 4 refresh hook may
    // legitimately rewrite them mid-session; anything else changing
    // them is a smell worth surfacing.
    if let Some(snap) = creds.snapshot.as_ref() {
        crate::secrets::verify_snapshot(snap);
    }

    Ok(exit)
}

/// Single-quote `s` for use as a single argv element in a `bash -c`
/// line. Embedded single quotes are split out with the standard
/// `'\''` trick. Adequate for forwarding arbitrary user-supplied agent
/// args through the resolv.conf prelude wrapper.
fn shell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_escape_handles_simple_and_quoted() {
        assert_eq!(shell_escape("foo"), "'foo'");
        assert_eq!(shell_escape("--flag=value with spaces"), "'--flag=value with spaces'");
        assert_eq!(shell_escape("don't"), "'don'\\''t'");
        assert_eq!(shell_escape(""), "''");
    }
}

async fn notify_if_update_available(image: &str) {
    use crate::image_check::{UpdateState, check_for_update};
    match check_for_update(image).await {
        Ok(Some(UpdateState::UpdateAvailable { cached, remote })) => {
            eprintln!(
                "==> A newer image is available in the registry (cached {cached}, registry {remote})"
            );
            eprintln!(
                "==> Run `agent-vm pull` to fetch it. Continuing with the cached image."
            );
        }
        // UpToDate / NotCached: nothing to say.
        // None / Err: registry unreachable etc. — stay quiet.
        _ => {}
    }
}

