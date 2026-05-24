//! `agent-vm clipboard {get,put}` — file-backed clipboard exchange
//! between host and the agent inside the per-project sandbox.
//!
//! Design: per-project `<state>/clipboard.txt` is bind-mounted at
//! `/agent-vm-state/clipboard.txt` inside the guest. The agent
//! reads/writes that path directly; the user on the host runs
//! `agent-vm clipboard {get,put}` from the project dir to exchange.
//!
//! `get` prints the file's content to stdout. `put` reads stdin (or
//! a single positional arg) into the file. Both also exchange with
//! the system clipboard when `--sys`/`-s` is passed — host-side it's
//! `xclip` (X11) or `wl-copy`/`wl-paste` (Wayland) or
//! `pbcopy`/`pbpaste` (macOS), whichever is on PATH.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use clap::{Args as ClapArgs, Subcommand};

use crate::host_paths::atomic_write;
use crate::session::ProjectSession;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    op: Op,
}

#[derive(Subcommand)]
enum Op {
    /// Print the project clipboard's contents to stdout.
    Get {
        /// Also mirror the value to the host system clipboard.
        #[arg(long, short = 's')]
        sys: bool,
    },
    /// Read stdin (or VALUE) into the project clipboard.
    Put {
        /// Single-string value to put; otherwise stdin is read.
        value: Option<String>,
        /// Also pull from the host system clipboard before writing
        /// (instead of from stdin / `value`).
        #[arg(long, short = 's')]
        sys: bool,
    },
}

pub fn run(args: Args) -> Result<()> {
    let session = ProjectSession::for_cwd()?;
    session.ensure_dirs()?;
    let path = clipboard_path(&session);

    match args.op {
        Op::Get { sys } => {
            let bytes = std::fs::read(&path).unwrap_or_default();
            std::io::stdout()
                .write_all(&bytes)
                .context("writing to stdout")?;
            if sys {
                send_to_system_clipboard(&bytes).context("syncing to system clipboard")?;
            }
        }
        Op::Put { value, sys } => {
            let bytes = if sys {
                read_from_system_clipboard().context("reading system clipboard")?
            } else if let Some(v) = value {
                v.into_bytes()
            } else {
                let mut buf = Vec::new();
                std::io::stdin()
                    .read_to_end(&mut buf)
                    .context("reading stdin")?;
                buf
            };
            atomic_write(&path, &bytes, 0o600)
                .with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(())
}

/// Project clipboard file path. The launcher bind-mounts the
/// parent (`state_dir`) at `/agent-vm-state` so the guest sees
/// this at `/agent-vm-state/clipboard.txt`.
pub fn clipboard_path(session: &ProjectSession) -> PathBuf {
    session.state_dir.join("clipboard.txt")
}

/// First non-empty system-clipboard tool on PATH writes `bytes` via
/// stdin. Silently no-op if no tool is available.
fn send_to_system_clipboard(bytes: &[u8]) -> Result<()> {
    for (cmd, args) in [
        ("wl-copy", vec![]),
        ("xclip", vec!["-selection", "clipboard"]),
        ("pbcopy", vec![]),
    ] {
        if which(cmd).is_none() {
            continue;
        }
        let mut child = Command::new(cmd)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawning {cmd}"))?;
        if let Some(mut sin) = child.stdin.take() {
            sin.write_all(bytes).ok();
        }
        let _ = child.wait();
        return Ok(());
    }
    eprintln!(
        "warning: no system clipboard tool found on PATH (wl-copy / xclip / pbcopy); skipping --sys"
    );
    Ok(())
}

fn read_from_system_clipboard() -> Result<Vec<u8>> {
    for (cmd, args) in [
        ("wl-paste", vec!["--no-newline"]),
        ("xclip", vec!["-o", "-selection", "clipboard"]),
        ("pbpaste", vec![]),
    ] {
        if which(cmd).is_none() {
            continue;
        }
        let out = Command::new(cmd)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .with_context(|| format!("running {cmd}"))?;
        if out.status.success() {
            return Ok(out.stdout);
        }
    }
    anyhow::bail!(
        "no system clipboard tool found on PATH (wl-paste / xclip / pbpaste); pass input on stdin or as a positional arg"
    );
}

fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let full = dir.join(cmd);
        if full.is_file() {
            return Some(full);
        }
    }
    None
}
