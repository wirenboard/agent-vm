//! `agent-vm pull` — refresh the cached image without booting a sandbox.
//!
//! Microsandbox's SDK does not currently expose a "pull only" entry point;
//! the pull happens inside `Sandbox::create`. So we trigger one by booting
//! a throwaway sandbox with `PullPolicy::Always` and immediately stopping
//! it. The extra second of boot is a fair price for keeping pull logic in
//! a single place rather than reaching into microsandbox-image directly.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use microsandbox::{Image, Sandbox, sandbox::PullPolicy};

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

/// Force a pull of `image` into the microsandbox cache and exit. Used by
/// both `agent-vm pull` and the verify step in `agent-vm setup`.
///
/// We `Image::remove(force=true)` first because microsandbox's
/// `PullPolicy::Always` empirically re-fetches layer blobs but does *not*
/// update the cached manifest digest record in its DB. Without removing
/// the prior entry, subsequent `Image::get` calls still return the old
/// digest and `image_check` keeps showing the "update available" banner
/// forever. Removing then re-pulling makes the new manifest land cleanly.
pub async fn pull_image(image: &str) -> Result<()> {
    // PullPolicy::Always re-fetches layer blobs but doesn't update the
    // microsandbox manifest cache entry when the per-platform manifest
    // digest is unchanged. Removing first guarantees a clean re-pull.
    let _ = Image::remove(image, true).await;
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
    Ok(())
}
