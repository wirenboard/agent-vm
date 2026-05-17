//! Build and use a patched `msb` binary from `vendor/microsandbox`.
//!
//! Phase 4 turns on microsandbox features (`SecretValue::File`, the
//! request-interceptor hook) that don't exist in the upstream prebuilt
//! `~/.microsandbox/bin/msb`. We compile our own from the submodule
//! and point microsandbox's SDK at it via the `MSB_PATH` env var (top
//! of `microsandbox::config::resolve_msb_path`'s precedence ladder),
//! so the user's regular `~/.microsandbox/bin/msb` stays untouched
//! for any other tooling.
//!
//! The build is done by `agent-vm setup`; every other subcommand just
//! checks the binary exists and sets `MSB_PATH` if so.

use std::{path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};

/// Path where we expect our locally-built msb to live. The real CLI
/// binary lives in the `microsandbox-cli` crate (not `microsandbox` —
/// that crate's `microsandbox` bin is just a shim that forwards to
/// `~/.microsandbox/bin/msb`).
pub fn workspace_built_msb() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/microsandbox/target/release/msb")
}

/// If we have a built msb in the workspace, point microsandbox at it.
/// Quiet no-op otherwise (falls through to the SDK's normal resolution
/// chain — workspace_local, then `~/.microsandbox/bin/msb`, then
/// `$PATH`).
///
/// Safe to call multiple times. Does not override an explicit
/// user-set `MSB_PATH`.
pub fn point_at_workspace_msb() {
    if std::env::var_os("MSB_PATH").is_some() {
        return;
    }
    let p = workspace_built_msb();
    if !p.exists() {
        return;
    }
    // SAFETY: Called from main before any other thread is spawned.
    unsafe { std::env::set_var("MSB_PATH", p) };
}

/// Build the microsandbox CLI binary from the vendored submodule and
/// leave it at `workspace_built_msb()`. Called by `agent-vm setup`.
///
/// Skips if the built binary is already newer than the network crate's
/// source mtime — a heuristic but enough to avoid recompiling every
/// `setup` for unchanged source.
pub fn build_or_skip() -> Result<()> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/microsandbox/Cargo.toml")
        .canonicalize()
        .context("vendor/microsandbox not present; run `git submodule update --init vendor/microsandbox`")?;

    if msb_is_fresh(&manifest).unwrap_or(false) {
        println!("==> Patched msb already built; skipping (delete vendor/microsandbox/target/release/microsandbox to force rebuild)");
        return Ok(());
    }

    println!("==> Building patched msb from vendor/microsandbox (one-time, ~3-4 min)");
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "microsandbox-cli", "--bin", "msb"])
        .arg("--manifest-path")
        .arg(&manifest)
        .status()
        .context("invoking cargo build for vendor/microsandbox")?;
    if !status.success() {
        bail!("cargo build microsandbox failed: {status}");
    }
    let built = workspace_built_msb();
    if !built.exists() {
        bail!("cargo build succeeded but {} not found", built.display());
    }
    println!("==> Patched msb at {}", built.display());
    Ok(())
}

/// Heuristic freshness check: built msb exists and its mtime is newer
/// than the latest mtime under crates/network. Cheap, good enough for
/// "don't recompile if nothing changed since last `agent-vm setup`."
fn msb_is_fresh(microsandbox_manifest: &std::path::Path) -> Result<bool> {
    let built = workspace_built_msb();
    if !built.exists() {
        return Ok(false);
    }
    let built_mtime = std::fs::metadata(&built)?.modified()?;
    let network_dir = microsandbox_manifest
        .parent()
        .context("manifest has no parent")?
        .join("crates/network/lib");
    let latest_src = walk_latest_mtime(&network_dir)?;
    Ok(built_mtime >= latest_src)
}

fn walk_latest_mtime(root: &std::path::Path) -> Result<std::time::SystemTime> {
    let mut latest = std::time::SystemTime::UNIX_EPOCH;
    for entry in walkdir(root)? {
        let meta = std::fs::metadata(&entry)?;
        if meta.is_file()
            && let Ok(m) = meta.modified()
        {
            if m > latest {
                latest = m;
            }
        }
    }
    Ok(latest)
}

fn walkdir(root: &std::path::Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
        {
            let entry = entry?;
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    Ok(out)
}
