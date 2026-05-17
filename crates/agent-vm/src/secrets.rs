//! Host-rooted credentials.
//!
//! For each invocation we snapshot the host's Claude / Codex credential
//! files, write placeholder credentials into the guest-side state
//! directory, and return the real access tokens to the launcher. The
//! launcher registers them as microsandbox secret entries so the real
//! tokens are substituted into outgoing TLS-intercepted requests at the
//! network layer — the guest never sees them.
//!
//! Phase 3 captures the token **once at launch time** as a static
//! string. Long-running sandboxes will lose auth when the host token
//! expires; the access-token-rotation work lives in Phase 4 along with
//! the cross-process plumbing needed for live file-backed secrets
//! (SecretValue::File exists on our microsandbox fork branch but the
//! prebuilt msb runtime daemon on the host hasn't been rebuilt to
//! understand it yet).
//!
//! Placeholders are stable per-version so credentials JSON written by a
//! prior invocation is still valid for the current one.

use std::{fs, path::Path, path::PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

pub const ANTHROPIC_PLACEHOLDER: &str = "MSB_PLACEHOLDER_ANTHROPIC_ACCESS_TOKEN_v1";
pub const OPENAI_PLACEHOLDER: &str = "MSB_PLACEHOLDER_OPENAI_ACCESS_TOKEN_v1";

/// Result of [`refresh`]. `*_access_token` strings are only present if the
/// host credential file was found and parsed successfully. They are
/// short-lived — handed to the network builder and dropped once the
/// sandbox config is materialized; we never log them.
#[derive(Default)]
pub struct CredsState {
    pub anthropic_access_token: Option<String>,
    pub openai_access_token: Option<String>,
}

impl std::fmt::Debug for CredsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredsState")
            .field(
                "anthropic_access_token",
                &self.anthropic_access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "openai_access_token",
                &self.openai_access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Read host credentials and write placeholder guest credentials. Returns
/// access tokens that the launcher passes to microsandbox as static
/// secret values. Silently skips providers whose host file is missing or
/// unparseable so a host with only Claude credentials still works.
pub fn refresh(state_dir: &Path) -> Result<CredsState> {
    let anthropic_access_token = match refresh_anthropic(state_dir) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "anthropic credential refresh failed; skipping");
            None
        }
    };
    let openai_access_token = match refresh_openai(state_dir) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "openai credential refresh failed; skipping");
            None
        }
    };

    Ok(CredsState {
        anthropic_access_token,
        openai_access_token,
    })
}

fn refresh_anthropic(state_dir: &Path) -> Result<Option<String>> {
    let Some(host_path) = host_claude_creds_path() else {
        return Ok(None);
    };
    if !host_path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&host_path)
        .with_context(|| format!("reading {}", host_path.display()))?;
    let json: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", host_path.display()))?;

    let oauth = json
        .get("claudeAiOauth")
        .context("host .credentials.json missing claudeAiOauth")?;
    let access_token = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())
        .context("host claudeAiOauth missing accessToken")?
        .to_string();

    // Placeholder credentials.json the in-guest Claude reads. Goes
    // into <state>/claude which is symlinked from /root/.claude in
    // the guest (Phase 2 wiring).
    let claude_dir = state_dir.join("claude");
    fs::create_dir_all(&claude_dir)?;
    let placeholder = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": ANTHROPIC_PLACEHOLDER,
            "refreshToken": "REFRESH_NOT_AVAILABLE_PHASE_3",
            "expiresAt": oauth.get("expiresAt"),
            "scopes": oauth.get("scopes"),
            "subscriptionType": oauth.get("subscriptionType"),
            "rateLimitTier": oauth.get("rateLimitTier"),
        }
    });
    let body = serde_json::to_string_pretty(&placeholder)?;
    atomic_write(&claude_dir.join(".credentials.json"), body.as_bytes(), 0o600)?;

    Ok(Some(access_token))
}

fn refresh_openai(state_dir: &Path) -> Result<Option<String>> {
    let Some(host_path) = host_codex_auth_path() else {
        return Ok(None);
    };
    if !host_path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&host_path)
        .with_context(|| format!("reading {}", host_path.display()))?;
    let mut json: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", host_path.display()))?;

    // Either ChatGPT OAuth (`tokens.access_token`) or an API key
    // (`OPENAI_API_KEY`). Both end up as Bearer in outgoing requests.
    let access_token = json
        .pointer("/tokens/access_token")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("OPENAI_API_KEY").and_then(|v| v.as_str()))
        .context("host codex auth missing tokens.access_token or OPENAI_API_KEY")?
        .to_string();

    // Replace the real value in-place with the placeholder, preserving
    // every other field (account_id, last_refresh, etc.) so the in-VM
    // codex sees a valid-looking auth.json shape.
    if let Some(tokens) = json.get_mut("tokens").and_then(|v| v.as_object_mut()) {
        if tokens.contains_key("access_token") {
            tokens.insert(
                "access_token".into(),
                Value::String(OPENAI_PLACEHOLDER.to_string()),
            );
        }
        if tokens.contains_key("refresh_token") {
            tokens.insert(
                "refresh_token".into(),
                Value::String("REFRESH_NOT_AVAILABLE_PHASE_3".to_string()),
            );
        }
    }
    if json.get("OPENAI_API_KEY").is_some() {
        json["OPENAI_API_KEY"] = Value::String(OPENAI_PLACEHOLDER.to_string());
    }
    let body = serde_json::to_string_pretty(&json)?;

    let codex_dir = state_dir.join("codex");
    fs::create_dir_all(&codex_dir)?;
    atomic_write(&codex_dir.join("auth.json"), body.as_bytes(), 0o600)?;

    Ok(Some(access_token))
}

fn host_claude_creds_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude/.credentials.json"))
}

fn host_codex_auth_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".codex/auth.json"))
}

/// Atomic write-then-rename with mode 0600. Prevents readers from ever
/// seeing a half-written file.
fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let tmp = path.with_extension("agent-vm-tmp");
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("opening {}", tmp.display()))?;
        f.write_all(data)
            .with_context(|| format!("writing {}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}
