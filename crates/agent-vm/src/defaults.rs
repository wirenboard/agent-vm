//! Module-level constants for distribution-shaped defaults.
//!
//! Kept in one place so a release can re-point the image registry,
//! bump the image-API range, or change other distribution wiring
//! without grepping for string literals across subcommands.

/// Default OCI image reference. Overridable per-subcommand via
/// `--image` or the `AGENT_VM_IMAGE_TAG` env var. Pulled fresh on
/// `agent-vm setup` / `agent-vm pull`; uses the cached copy
/// otherwise.
///
/// Tags published by CI:
/// - `:latest` — moving tag, rebuilt hourly to pick up agent
///   updates (Claude Code, OpenCode, Codex etc.).
/// - `:YYYY-MM-DDTHH` — timestamped, immutable. Use for
///   reproducible setups.
pub const DEFAULT_IMAGE_REF: &str = "ghcr.io/wirenboard/agent-vm:latest";

/// Image-API contract version range this binary supports.
///
/// The image writes `/etc/agent-vm-image-version` containing a
/// single integer N (see `images/Dockerfile`). On first connect
/// agent-vm reads it and requires
/// `MIN_SUPPORTED_IMAGE_API <= N <= MAX_SUPPORTED_IMAGE_API` —
/// otherwise it refuses to launch with a clear "image
/// too new / too old, update <one side>" message.
///
/// Bump on breaking changes only: new required mount points,
/// changed env-var contracts, removed in-VM binaries, etc.
/// Routine updates of agent versions don't bump this.
pub const MIN_SUPPORTED_IMAGE_API: u32 = 1;
pub const MAX_SUPPORTED_IMAGE_API: u32 = 1;

/// Path the image writes its API version to. Read by agent-vm
/// from inside the guest immediately after boot.
pub const IMAGE_API_VERSION_PATH: &str = "/etc/agent-vm-image-version";
