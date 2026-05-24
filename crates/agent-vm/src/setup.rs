//! `agent-vm setup` — pull the base OCI image and verify it under microsandbox.
//!
//! The image is hosted on a registry that CI publishes on a separate
//! cadence (see `.github/workflows/build-image.yml`). Setup just
//! pulls into microsandbox's cache and verifies by booting a
//! throwaway sandbox.
//!
//! Source-checkout users can build a local image with
//! `images/build.sh` and point setup at it via
//! `--image localhost:5000/agent-vm:latest`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use microsandbox::{Sandbox, sandbox::PullPolicy};

#[derive(ClapArgs)]
pub struct Args {
    /// Skip the post-pull verification sandbox.
    #[arg(long)]
    no_verify: bool,

    /// Override the image reference. Defaults to
    /// `ghcr.io/wirenboard/agent-vm:latest`. Source-checkout users
    /// who built a local image can point at it
    /// (`--image localhost:5000/agent-vm:latest`) — agent-vm
    /// detects local registries and uses plain HTTP.
    #[arg(long, env = "AGENT_VM_IMAGE_TAG")]
    image: Option<String>,
}

pub async fn run(args: Args) -> Result<()> {
    let image = args
        .image
        .unwrap_or_else(|| crate::defaults::DEFAULT_IMAGE_REF.to_string());

    // Source-checkout dev workflow: rebuild the patched msb from the
    // vendored microsandbox submodule if present. npm-installed
    // agent-vm has no submodule and main()'s `point_at_msb` already
    // discovered the prebuilt sibling — skip the rebuild entirely.
    let vendor_present = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/microsandbox/Cargo.toml")
        .exists();
    if vendor_present {
        crate::msb_install::build_or_skip()?;
        crate::msb_install::point_at_msb()?;
    }

    println!("==> Pulling {image} into the microsandbox cache");
    crate::pull::pull_image(&image).await?;

    if !args.no_verify {
        verify_image(&image).await?;
    }

    println!("==> {image} ready");
    Ok(())
}

async fn verify_image(image: &str) -> Result<()> {
    println!("==> Verifying {image}");
    println!("==> Booting throwaway sandbox (this is the first VM cold-start; ~3s on a warm host)");
    // The pull step above already pulled the new manifest, so IfMissing
    // is fine here.
    let is_local = crate::pull::is_plain_http_registry(image);
    let config = Sandbox::builder("agent-vm-setup-verify")
        .image(image)
        .registry(|r| if is_local { r.insecure() } else { r })
        .pull_policy(PullPolicy::IfMissing)
        .cpus(1)
        .memory(512)
        .replace()
        .build()
        .await
        .context("preparing verify config")?;
    let (progress, task) = Sandbox::create_with_pull_progress(config);
    let render_task = tokio::spawn(crate::pull_progress::render(progress));
    let sandbox = task
        .await
        .context("create-with-pull-progress join")?
        .context("booting verify sandbox")?;
    render_task.await.ok();

    println!("==> Checking image API version and agent --versions inside the sandbox");
    let out = sandbox
        .shell(&format!(
            "cat {} && claude --version && opencode --version && codex --version",
            crate::defaults::IMAGE_API_VERSION_PATH
        ))
        .await
        .context("running version checks inside sandbox")?;

    println!("{}", out.stdout()?.trim_end());

    println!("==> Stopping verify sandbox");
    sandbox.stop_and_wait().await.ok();
    Sandbox::remove("agent-vm-setup-verify").await.ok();

    let code = out.status().code;
    if code != 0 {
        bail!(
            "image verification exited with {code}. \
             If `{}` was the missing file, this image is too old for your \
             agent-vm; update the binary or pin to a newer image tag.",
            crate::defaults::IMAGE_API_VERSION_PATH,
        );
    }
    Ok(())
}
