//! Shared host-side path helpers used by several modules.
//!
//! Centralized here so `state_root` / host credential paths /
//! `atomic_write` semantics stay aligned between the launcher
//! (`run.rs`, `secrets.rs`) and the interceptor hook
//! (`intercept_hook.rs`), and the pulled-marker file
//! (`pulled_marker.rs`).

use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

/// `$AGENT_VM_STATE_DIR` → `$XDG_STATE_HOME/agent-vm` → `$HOME/.local/state/agent-vm`.
/// Returns `None` only if `$HOME` is unset (rare; corrupted env).
pub fn state_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("AGENT_VM_STATE_DIR") {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(dir).join("agent-vm"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/state/agent-vm"))
}

/// `$HOME/.claude/.credentials.json`. The file may not exist; callers
/// should treat `ErrorKind::NotFound` on read as "no Claude creds".
pub fn host_claude_creds_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".claude/.credentials.json"))
}

/// `$HOME/.codex/auth.json`. Same not-found convention as
/// [`host_claude_creds_path`].
pub fn host_codex_auth_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".codex/auth.json"))
}

/// `$HOME/.local/share/opencode/auth.json`. Same not-found convention.
/// OpenCode auths against OpenAI like Codex does, but stores its own
/// auth file in the XDG data dir.
pub fn host_opencode_auth_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/share/opencode/auth.json"))
}

/// `$HOME/.cache/claude-vm/copilot-token.json` — the GitHub Copilot
/// token cache the original Bash agent-vm's `copilot_token.py` writes
/// after its OAuth device flow (JSON `{"access_token": "<gho_…>"}`).
/// Same not-found convention as the other host credential helpers:
/// callers treat a missing file as "no cached Copilot token" and fall
/// back to the captured `gh auth token`.
pub fn host_copilot_token_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".cache/claude-vm/copilot-token.json"))
}

/// Write `data` to `path` atomically (write a sibling tmp file, then
/// `rename`) with the given Unix mode. The tmp file uses a fixed
/// extension so a crashed run leaves an obvious orphan rather than a
/// half-written target.
pub fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let tmp = path.with_extension("agent-vm-tmp");
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("opening {}", tmp.display()))?;
        f.write_all(data)
            .with_context(|| format!("writing {}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}
