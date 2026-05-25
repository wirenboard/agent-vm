//! `agent-vm pull` — refresh the cached image without booting a real
//! sandbox.
//!
//! Microsandbox's SDK does not currently expose a "pull only" entry
//! point; the pull happens inside `Sandbox::create`. So we trigger one
//! by booting a throwaway sandbox with `PullPolicy::Always` and
//! immediately stopping it.
//!
//! Cache integrity note: we deliberately do **not** call `Image::remove`
//! before the pull (the previous design did, to work around a
//! microsandbox bug where `Image::get` keeps returning the prior
//! manifest digest under the same reference). The removal opens a
//! cache-empty window — if the user ^C's mid-pull, or the registry
//! disappears, the next `agent-vm shell` triggers a full IfMissing
//! re-pull from scratch. Instead we keep microsandbox's cache untouched
//! and track "the digest we successfully pulled" in our own marker file
//! (`pulled_marker`), updated only after the pull completes. The cache
//! is therefore always usable; only the update-available banner may be
//! briefly wrong if interrupted, and it self-corrects on the next pull.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use microsandbox::{Sandbox, sandbox::PullPolicy};

#[derive(ClapArgs)]
pub struct Args {
    /// Override the image reference. Defaults to
    /// `ghcr.io/wirenboard/agent-vm:latest` or the value of
    /// `AGENT_VM_IMAGE_TAG`. Use a timestamped tag
    /// (`...:YYYY-MM-DDTHH`) to pin a specific build.
    #[arg(long, env = "AGENT_VM_IMAGE_TAG")]
    image: Option<String>,
}

pub async fn run(args: Args) -> Result<()> {
    let image = args
        .image
        .unwrap_or_else(|| crate::defaults::DEFAULT_IMAGE_REF.to_string());
    pull_image(&image).await?;
    println!("==> {image} pulled into the microsandbox cache");
    Ok(())
}

/// Force a pull of `image` into the microsandbox cache and exit. Used
/// by both `agent-vm pull` and the verify step in `agent-vm setup`.
pub async fn pull_image(image: &str) -> Result<()> {
    let is_local = is_plain_http_registry(image);
    let config = Sandbox::builder("agent-vm-pull")
        .image(image)
        .registry(|r| if is_local { r.insecure() } else { r })
        .pull_policy(PullPolicy::Always)
        .cpus(1)
        .memory(256)
        .replace()
        .build()
        .await
        .context("preparing pull config")?;
    let (progress, task) = Sandbox::create_with_pull_progress(config);
    let render = tokio::spawn(crate::pull_progress::render(progress));
    let sandbox = task
        .await
        .context("pull task join")?
        .context("pulling image")?;
    render.await.ok();
    sandbox.stop_and_wait().await.ok();
    Sandbox::remove("agent-vm-pull").await.ok();

    // Only after a successful pull do we record what we landed. If
    // anything above failed, the marker is unchanged and the next
    // launch's banner still flags this image as needing an update.
    if let Some(digest) = crate::image_check::fetch_remote_digest(image).await {
        if let Err(e) = crate::pulled_marker::write(image, &digest) {
            eprintln!("warn: failed to record pulled digest: {e}");
        }
    }

    Ok(())
}

/// Decide whether the registry needs `--insecure` (plain HTTP).
///
/// Two ways to get a `true`:
/// 1. `AGENT_VM_INSECURE_REGISTRY=1` env var — opt-in escape hatch
///    for airgapped/intranet plain-HTTP registries
///    (`registry.corp.example:5000`, `192.168.1.10:5000`, etc.) that
///    the heuristic can't recognise from the ref alone.
/// 2. Hostname is unambiguously local: `localhost`, `127.0.0.1`,
///    `0.0.0.0`, `*.local`, `*.localhost`.
///
/// The microsandbox SDK's `.insecure()` switches to plain HTTP, so
/// applying it to ghcr.io or any other public HTTPS registry would
/// break the pull. The heuristic stays narrow for safety; the env
/// var is the operator-blessed override.
///
/// IPv6 literal hosts are not supported here (rare for local dev
/// registries; bracketed form would need a real parser). Use
/// `localhost` or `127.0.0.1` instead.
pub(crate) fn is_plain_http_registry(image_ref: &str) -> bool {
    if env_truthy("AGENT_VM_INSECURE_REGISTRY") {
        return true;
    }
    // First path segment is the registry host (with optional :port).
    // If there's no `/` in the ref OR the first segment has no `.` /
    // `:` / `localhost`, the ref points at docker.io's default
    // registry and we never want insecure for that.
    let first = image_ref.split('/').next().unwrap_or("");
    // Strip the port suffix to get just the host.
    let host = first.split(':').next().unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0")
        || host.ends_with(".local")
        || host.ends_with(".localhost")
}

fn env_truthy(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_registries_are_plain_http() {
        assert!(is_plain_http_registry("localhost:5000/agent-vm:latest"));
        assert!(is_plain_http_registry("127.0.0.1:5000/x"));
        assert!(is_plain_http_registry("0.0.0.0:8080/x"));
        assert!(is_plain_http_registry("dev.local/x"));
        assert!(is_plain_http_registry("foo.localhost:5000/x"));
    }

    #[test]
    fn public_registries_are_not_plain_http() {
        assert!(!is_plain_http_registry("ghcr.io/wirenboard/agent-vm:latest"));
        assert!(!is_plain_http_registry("docker.io/library/debian:13"));
        assert!(!is_plain_http_registry("registry.example.com/x"));
        // Docker Hub short form has no `/` in the registry part.
        assert!(!is_plain_http_registry("nginx:latest"));
    }

    // Single test exercising the env-var escape hatch — the heuristic
    // says "secure" for `registry.corp.example` but the env override
    // wins. Uses a serial-style guard (set + clear) so a parallel test
    // doesn't observe the var. (cargo test runs tests in parallel by
    // default — keep the var name unique to this test so it can't
    // collide with another module setting the same var.)
    #[test]
    fn env_override_forces_plain_http() {
        // SAFETY: cargo test parallelises but env mutations affect the
        // whole process; restrict to one assertion + cleanup. The
        // assertions in the OTHER tests in this module don't touch
        // AGENT_VM_INSECURE_REGISTRY, so no interference.
        // SAFETY: see rationale above.
        unsafe { std::env::set_var("AGENT_VM_INSECURE_REGISTRY", "1") };
        assert!(is_plain_http_registry("registry.corp.example:5000/x"));
        assert!(is_plain_http_registry("ghcr.io/wirenboard/agent-vm:latest"));
        // SAFETY: same.
        unsafe { std::env::remove_var("AGENT_VM_INSECURE_REGISTRY") };
        // After cleanup the heuristic resumes its normal behaviour.
        assert!(!is_plain_http_registry("registry.corp.example:5000/x"));
    }
}
