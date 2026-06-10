//! Per-project session state.
//!
//! Each project directory gets a stable hash → state directory under
//! `${XDG_STATE_HOME:-~/.local/state}/agent-vm/<hash>/`. Agent-specific
//! subdirectories under that root are bind-mounted into the guest at the
//! standard paths (`/root/.claude`, `/root/.codex`,
//! `/root/.local/share/opencode`) so session history survives across runs.
//!
//! The sandbox *name* additionally carries the launcher PID
//! (`agent-vm-<hash>-<pid>`) so two concurrent `agent-vm` invocations
//! from the same project boot independent VMs. Without this, the second
//! launch's `Sandbox::create` would SIGTERM/SIGKILL the first one's VMM
//! (we used to set `.replace()` to handle the same-name collision; now
//! there is no collision to handle). Per-project bind-mounted state
//! (claude/, codex/, opencode/, bash_history) is still shared between
//! the two — running two agents that mutate the same session files at
//! once is the user's call.

use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Everything Phase 2 needs to know about a project invocation.
pub struct ProjectSession {
    pub project_dir: PathBuf,
    pub project_hash: String,
    pub state_dir: PathBuf,
    pub sandbox_name: String,
}

impl ProjectSession {
    /// Build a session rooted at the current working directory.
    pub fn for_cwd() -> Result<Self> {
        let project_dir = env::current_dir()
            .context("reading current directory")?
            .canonicalize()
            .context("canonicalizing current directory")?;
        Self::for_dir(project_dir)
    }

    fn for_dir(project_dir: PathBuf) -> Result<Self> {
        let project_hash = hash_path(&project_dir);
        let state_dir = state_root()?.join(&project_hash);
        // Per-launch PID suffix so two concurrent invocations in the same
        // project boot independent sandboxes (the first one stays alive
        // instead of being SIGTERMed by the second's create()). The PID
        // is unique across currently-running processes on the host, which
        // is the only collision window we need to handle — a leftover
        // sandbox from a crashed launcher is cleaned up by the
        // Sandbox::remove call at end of launch().
        let sandbox_name = format!("agent-vm-{project_hash}-{}", std::process::id());
        Ok(Self {
            project_dir,
            project_hash,
            state_dir,
            sandbox_name,
        })
    }

    /// Create the state subdirectories that will be bind-mounted into the
    /// guest. Called before sandbox creation so virtiofs has somewhere real to
    /// point at.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.state_dir,
            &self.claude_home(),
            &self.codex_home(),
            &self.opencode_data(),
        ] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn claude_home(&self) -> PathBuf {
        self.state_dir.join("claude")
    }

    pub fn codex_home(&self) -> PathBuf {
        self.state_dir.join("codex")
    }

    pub fn opencode_data(&self) -> PathBuf {
        self.state_dir.join("opencode")
    }
}

fn state_root() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("AGENT_VM_STATE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(dir).join("agent-vm"));
    }
    let home = env::var_os("HOME").context("no $HOME set")?;
    Ok(PathBuf::from(home).join(".local/state/agent-vm"))
}

/// 12-hex-char prefix of SHA256(canonical_path). Short enough to keep sandbox
/// names readable; long enough that two project dirs are very unlikely to
/// collide on the same host.
fn hash_path(path: &Path) -> String {
    let mut h = Sha256::new();
    h.update(path.as_os_str().as_encoded_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(12);
    for byte in &digest[..6] {
        use std::fmt::Write;
        write!(&mut s, "{byte:02x}").unwrap();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_short() {
        let h = hash_path(Path::new("/some/project"));
        assert_eq!(h.len(), 12);
        assert_eq!(h, hash_path(Path::new("/some/project")));
        assert_ne!(h, hash_path(Path::new("/other/project")));
    }

    #[test]
    fn sandbox_name_carries_pid_for_concurrent_safety() {
        // Two ProjectSession values for the same dir must share state
        // (so per-project history etc. survives) but produce distinct
        // sandbox names within the same process (PID disambiguator —
        // and across processes the PIDs differ by definition).
        let dir = std::env::temp_dir();
        let a = ProjectSession::for_dir(dir.clone()).expect("for_dir a");
        let b = ProjectSession::for_dir(dir.clone()).expect("for_dir b");
        assert_eq!(a.state_dir, b.state_dir);
        assert_eq!(a.project_hash, b.project_hash);
        // Same process → same PID → same name within-process. The
        // concurrent-launch guarantee comes from PIDs differing across
        // processes; assert the name format encodes the PID so a future
        // refactor that drops it from the format trips the test.
        let pid = std::process::id().to_string();
        assert!(
            a.sandbox_name.ends_with(&format!("-{pid}")),
            "sandbox_name {:?} must end with -<pid>",
            a.sandbox_name
        );
        assert_eq!(a.sandbox_name, b.sandbox_name);
    }
}
