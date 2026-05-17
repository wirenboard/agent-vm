//! agent-vm — sandboxed VMs for AI coding agents.
//!
//! Phase 0: hello-world. Boots a throwaway alpine microsandbox, runs `echo`,
//! and exits. This exists only to prove the SDK wiring is correct end-to-end.
//! Real subcommands land in Phase 2.

use microsandbox::Sandbox;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = Sandbox::builder("agent-vm-hello")
        .image("alpine")
        .cpus(1)
        .memory(256)
        .replace()
        .create()
        .await?;

    let output = sandbox.shell("echo hello from alpine").await?;
    println!("{}", output.stdout()?.trim_end());

    sandbox.stop_and_wait().await?;
    Sandbox::remove("agent-vm-hello").await?;
    Ok(())
}
