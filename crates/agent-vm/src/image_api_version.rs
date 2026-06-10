//! Read and verify the image-API contract version baked into the
//! agent-vm OCI image at boot.
//!
//! The image is expected to ship a file containing a single integer
//! (e.g. `1\n`) at [`crate::defaults::IMAGE_API_VERSION_PATH`]. The
//! integer encodes the *interface* between agent-vm and the image:
//! mount points the binary expects, env-var contracts, in-VM
//! script entry points, etc. Routine refreshes of agent versions
//! (Claude Code, Codex, OpenCode) do NOT bump it.
//!
//! On every launch agent-vm reads it and requires
//!
//!     MIN_SUPPORTED_IMAGE_API <= N <= MAX_SUPPORTED_IMAGE_API
//!
//! Missing file or out-of-range value → refuse to launch with an
//! actionable error pointing the user at the right side to update.
//!
//! Without this check a binary/image mismatch produces inscrutable
//! runtime failures (file-not-found, weird path errors, agents
//! crashing inside the VM) instead of one clear line at launch.

use anyhow::{Context, Result, bail};
use microsandbox::Sandbox;

use crate::defaults::{
    IMAGE_API_VERSION_PATH, MAX_SUPPORTED_IMAGE_API, MIN_SUPPORTED_IMAGE_API,
};

/// Read `IMAGE_API_VERSION_PATH` inside `sandbox`, parse the integer,
/// and require it to be within the supported range. Returns the
/// successfully verified version number; bails on any deviation.
pub async fn check(sandbox: &Sandbox) -> Result<u32> {
    // Single shell invocation: `cat` the file. If the file is
    // missing, cat exits non-zero and we surface a "too old" error.
    let out = sandbox
        .shell(&format!("cat {}", IMAGE_API_VERSION_PATH))
        .await
        .with_context(|| {
            format!(
                "reading {IMAGE_API_VERSION_PATH} from the guest sandbox"
            )
        })?;

    let stdout = out.stdout().context("decoding shell output")?;
    let code = out.status().code;

    if code != 0 {
        bail!(
            "image is missing {IMAGE_API_VERSION_PATH} (read exited {code}).\n\
             This image is too old for this agent-vm binary. Pull the latest \
             image with `agent-vm pull` or pin an older agent-vm release."
        );
    }

    let version = parse(&stdout).with_context(|| {
        format!(
            "parsing image-API version from {IMAGE_API_VERSION_PATH} \
             (got {stdout:?})"
        )
    })?;

    verify_in_range(version)?;
    Ok(version)
}

/// Trim + parse the version integer. Tolerates trailing whitespace
/// from `echo`/`cat`. Rejects empty input and non-integer content.
fn parse(s: &str) -> Result<u32> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        bail!("empty");
    }
    trimmed
        .parse::<u32>()
        .with_context(|| format!("not an integer: {trimmed:?}"))
}

fn verify_in_range(version: u32) -> Result<()> {
    if version < MIN_SUPPORTED_IMAGE_API {
        bail!(
            "image-API version {version} is too OLD \
             (this agent-vm needs {MIN_SUPPORTED_IMAGE_API}..={MAX_SUPPORTED_IMAGE_API}).\n\
             Pull the latest image with `agent-vm pull`."
        );
    }
    if version > MAX_SUPPORTED_IMAGE_API {
        bail!(
            "image-API version {version} is too NEW \
             (this agent-vm supports {MIN_SUPPORTED_IMAGE_API}..={MAX_SUPPORTED_IMAGE_API}).\n\
             Update agent-vm (`npm install -g @wirenboard/agent-vm@latest`) \
             or pin an older image tag via `--image ghcr.io/wirenboard/agent-vm-template:YYYY-MM-DDTHH`."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_trailing_whitespace() {
        assert_eq!(parse("1\n").unwrap(), 1);
        assert_eq!(parse(" 42 \n").unwrap(), 42);
        assert_eq!(parse("0").unwrap(), 0);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse("").is_err());
        assert!(parse("abc").is_err());
        assert!(parse("-1").is_err()); // u32 can't be negative
        assert!(parse("1.5").is_err());
    }

    #[test]
    fn verify_accepts_in_range() {
        for v in MIN_SUPPORTED_IMAGE_API..=MAX_SUPPORTED_IMAGE_API {
            verify_in_range(v).expect("in-range version must pass");
        }
    }

    #[test]
    fn verify_rejects_too_new_with_hint() {
        let err = verify_in_range(MAX_SUPPORTED_IMAGE_API + 1).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("too NEW"));
        assert!(msg.contains("npm install"));
    }

    #[test]
    fn verify_rejects_too_old_with_hint_if_possible() {
        // MIN can be 0; in that case nothing is "too old". Skip in
        // that case — the error path is reachable only when MIN > 0,
        // which we'll bump on first breaking image change.
        if MIN_SUPPORTED_IMAGE_API > 0 {
            let err = verify_in_range(MIN_SUPPORTED_IMAGE_API - 1).unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("too OLD"));
            assert!(msg.contains("agent-vm pull"));
        }
    }
}
