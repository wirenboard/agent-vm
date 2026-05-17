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
    /// Override the image reference. Defaults to localhost:5000/agent-vm:latest
    /// or the value of AGENT_VM_IMAGE_TAG.
    #[arg(long, env = "AGENT_VM_IMAGE_TAG")]
    image: Option<String>,
}

pub async fn run(args: Args) -> Result<()> {
    let image = args
        .image
        .unwrap_or_else(|| "localhost:5000/agent-vm:latest".to_string());
    pull_image(&image).await?;
    println!("==> {image} pulled into the microsandbox cache");
    Ok(())
}

/// Force a pull of `image` into the microsandbox cache and exit. Used
/// by both `agent-vm pull` and the verify step in `agent-vm setup`.
pub async fn pull_image(image: &str) -> Result<()> {
    let config = Sandbox::builder("agent-vm-pull")
        .image(image)
        .registry(|r| r.insecure())
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
