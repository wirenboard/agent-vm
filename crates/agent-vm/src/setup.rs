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
    //
    // If we appear to be running from a source checkout (the
    // CARGO_MANIFEST_DIR parent has `images/`) but the submodule is
    // NOT initialised, warn loudly: silently pulling ghcr.io is
    // surprising for someone who just edited `images/Dockerfile` and
    // expected `setup` to bake their changes in.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let vendor_present = manifest_dir
        .join("../../vendor/microsandbox/Cargo.toml")
        .exists();
    if vendor_present {
        crate::msb_install::build_or_skip()?;
        crate::msb_install::point_at_msb()?;
    } else if manifest_dir.join("../../images/Dockerfile").exists() {
        eprintln!(
            "warn: running from a source checkout but vendor/microsandbox is not \
             initialised — skipping local msb rebuild and pulling {image} from the \
             registry instead.\n\
             If you intended a local build, run \
             `git submodule update --init --recursive vendor/microsandbox`, \
             then `images/build.sh` and \
             `agent-vm setup --image localhost:5000/agent-vm:latest`."
        );
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

    // Image-API-version range check: same path agent-vm run takes
    // on every launch. Mismatch here = an actionable error at setup
    // time rather than a mysterious failure on first `agent-vm claude`.
    println!("==> Checking image-API contract version");
    crate::image_api_version::check(&sandbox)
        .await
        .with_context(|| {
            format!(
                "image-API check during verify failed; {image} is not compatible with this agent-vm"
            )
        })?;

    // Per-agent --version checks, run independently so the error
    // names which one fails instead of a generic && short-circuit.
    println!("==> Checking in-VM agent versions");
    for (name, cmd) in [
        ("claude", "claude --version"),
        ("opencode", "opencode --version"),
        ("codex", "codex --version"),
    ] {
        let out = sandbox
            .shell(cmd)
            .await
            .with_context(|| format!("running `{cmd}` inside sandbox"))?;
        let stdout = out.stdout()?;
        let trimmed = stdout.trim_end();
        if out.status().code != 0 {
            sandbox.stop_and_wait().await.ok();
            Sandbox::remove("agent-vm-setup-verify").await.ok();
            bail!(
                "`{cmd}` in {image} exited {} — the {name} agent is missing or broken in this image. \
                 Pull a newer tag (`agent-vm pull`) or report at \
                 https://github.com/wirenboard/agent-vm/issues. Output:\n{trimmed}",
                out.status().code
            );
        }
        println!("    {trimmed}");
    }

    println!("==> Stopping verify sandbox");
    sandbox.stop_and_wait().await.ok();
    Sandbox::remove("agent-vm-setup-verify").await.ok();

    Ok(())
}
