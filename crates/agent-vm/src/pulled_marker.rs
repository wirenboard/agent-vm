//! Side-channel record of the last digest we successfully pulled.
//!
//! Microsandbox's own cached manifest record doesn't update reliably
//! after a re-pull under the same tag (see notes in `pull.rs`), so we
//! can't use `Image::get(...).manifest_digest()` as the "what's in the
//! cache" signal for the update-available banner. Instead, after every
//! successful pull we atomically write the per-platform manifest digest
//! we just landed to a small marker file under XDG_STATE_HOME. The
//! launch-path update check reads this file.
//!
//! Atomicity matters: if a pull is interrupted, the marker file should
//! be either the *old* digest (so the next launch banner correctly flags
//! the update) or absent (treated as "not pulled yet"). The write-and-
//! rename pattern below guarantees we never see a half-written marker.

use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

pub fn read(image_ref: &str) -> Option<String> {
    let path = marker_path(image_ref)?;
    fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
}

pub fn write(image_ref: &str, digest: &str) -> Result<()> {
    let Some(path) = marker_path(image_ref) else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, digest).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("renaming {}", path.display()))?;
    Ok(())
}

fn marker_path(image_ref: &str) -> Option<PathBuf> {
    Some(state_root()?.join("pulled-digests").join(hash(image_ref)))
}

fn state_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("AGENT_VM_STATE_DIR") {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(dir).join("agent-vm"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/state/agent-vm"))
}

fn hash(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let d = h.finalize();
    let mut out = String::with_capacity(16);
    for b in &d[..8] {
        use std::fmt::Write;
        write!(out, "{b:02x}").unwrap();
    }
    out
}
