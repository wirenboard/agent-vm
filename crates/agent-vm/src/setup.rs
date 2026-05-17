//! `agent-vm setup` — build the base OCI image and verify it under microsandbox.
//!
//! The build step is delegated to `images/build.sh`, which knows how to run a
//! host-local Docker registry. The verify step boots a throwaway sandbox from
//! the freshly pushed image and runs each agent's `--version` to confirm the
//! image is actually usable end-to-end.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use microsandbox::{Sandbox, sandbox::PullPolicy};

#[derive(ClapArgs)]
pub struct Args {
    /// Skip the post-build verification sandbox.
    #[arg(long)]
    no_verify: bool,

    /// Override the image reference. Must point at a registry microsandbox can
    /// reach; the bundled build.sh defaults to localhost:5000/agent-vm:latest.
    #[arg(long, env = "AGENT_VM_IMAGE_TAG")]
    image: Option<String>,
}

pub async fn run(args: Args) -> Result<()> {
    let image = args
        .image
        .unwrap_or_else(|| "localhost:5000/agent-vm:latest".to_string());

    run_build_script()?;

    if !args.no_verify {
        verify_image(&image).await?;
    }

    println!("==> {image} ready");
    Ok(())
}

fn run_build_script() -> Result<()> {
    let script = build_script_path()?;
    let status = Command::new("bash")
        .arg(&script)
        .status()
        .with_context(|| format!("running {}", script.display()))?;
    if !status.success() {
        bail!("{} exited with {}", script.display(), status);
    }
    Ok(())
}

fn build_script_path() -> Result<PathBuf> {
    // Walk up from CARGO_MANIFEST_DIR (crates/agent-vm) to the repo root and
    // look for images/build.sh.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let candidate = manifest.join("../../images/build.sh");
    if candidate.exists() {
        return Ok(candidate.canonicalize()?);
    }
    bail!("images/build.sh not found relative to {}", manifest.display())
}

async fn verify_image(image: &str) -> Result<()> {
    println!("==> Verifying {image}");
    println!("==> Booting throwaway sandbox (this is the first VM cold-start; ~3s on a warm host)");
    // Pull policy: Always — we just rebuilt + pushed under the same tag,
    // so the cached manifest is by definition stale. Without this the
    // verify step boots the *previous* image and quietly attests that an
    // old version still works.
    let config = Sandbox::builder("agent-vm-setup-verify")
        .image(image)
        .registry(|r| r.insecure())
        .pull_policy(PullPolicy::Always)
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

    println!("==> Running claude/opencode/codex --version inside the sandbox");
    let out = sandbox
        .shell("claude --version && opencode --version && codex --version")
        .await
        .context("running version checks inside sandbox")?;

    println!("{}", out.stdout()?.trim_end());

    println!("==> Stopping verify sandbox");
    sandbox.stop_and_wait().await.ok();
    Sandbox::remove("agent-vm-setup-verify").await.ok();

    let code = out.status().code;
    if code != 0 {
        bail!("agent version check exited with {code}");
    }
    Ok(())
}
