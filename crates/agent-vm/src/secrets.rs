//! Host-rooted credentials.
//!
//! For each invocation we snapshot the host's Claude / Codex credential
//! files, write placeholder credentials into the guest-side state
//! directory, and return the *path* of a per-project token file to the
//! launcher. The launcher registers that path as a microsandbox
//! `SecretValue::File` entry so the patched msb re-reads it on every
//! connection-setup — host-side rotation is picked up on the next
//! request without rebuilding the sandbox.
//!
//! The token file is the same file the Phase 4 interceptor hook
//! rewrites whenever the in-VM agent asks for an OAuth refresh.
//!
//! Placeholders are stable per-version so credentials JSON written by
//! a prior invocation is still valid for the current one.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde_json::Value;

pub const ANTHROPIC_PLACEHOLDER: &str = "MSB_PLACEHOLDER_ANTHROPIC_ACCESS_TOKEN_v1";
pub const OPENAI_PLACEHOLDER: &str = "MSB_PLACEHOLDER_OPENAI_ACCESS_TOKEN_v1";

/// Result of [`refresh`]. `*_token_file` paths only exist if the host
/// credential file was found and parsed successfully.
#[derive(Debug, Default, Clone)]
pub struct CredsState {
    pub anthropic_token_file: Option<PathBuf>,
    pub openai_token_file: Option<PathBuf>,
}

/// Per-project location of the token file the proxy re-reads.
pub fn anthropic_token_path(state_dir: &Path) -> PathBuf {
    state_dir.join("tokens/anthropic")
}

pub fn openai_token_path(state_dir: &Path) -> PathBuf {
    state_dir.join("tokens/openai")
}

/// Read host credentials, write the token file (atomically, 0600) and
/// the guest-side placeholder credentials.json. Returns the paths to
/// the written token files so the launcher can plumb them into
/// microsandbox's SecretValue::File config.
pub fn refresh(state_dir: &Path) -> Result<CredsState> {
    fs::create_dir_all(state_dir.join("tokens"))?;
    let anthropic_token_file = match refresh_anthropic(state_dir) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "anthropic credential refresh failed; skipping");
            None
        }
    };
    let openai_token_file = match refresh_openai(state_dir) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "openai credential refresh failed; skipping");
            None
        }
    };

    Ok(CredsState {
        anthropic_token_file,
        openai_token_file,
    })
}

fn refresh_anthropic(state_dir: &Path) -> Result<Option<PathBuf>> {
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
        .context("host claudeAiOauth missing accessToken")?;

    // Token file the proxy re-reads on every connection-setup.
    let token_file = anthropic_token_path(state_dir);
    atomic_write(&token_file, access_token.as_bytes(), 0o600)?;

    // Placeholder credentials.json the in-guest Claude reads. Goes
    // into <state>/claude which is symlinked from /root/.claude in
    // the guest (Phase 2 wiring).
    let claude_dir = state_dir.join("claude");
    fs::create_dir_all(&claude_dir)?;
    let placeholder = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": ANTHROPIC_PLACEHOLDER,
            "refreshToken": "MSB_PLACEHOLDER_ANTHROPIC_REFRESH_TOKEN_v1",
            "expiresAt": oauth.get("expiresAt"),
            "scopes": oauth.get("scopes"),
            "subscriptionType": oauth.get("subscriptionType"),
            "rateLimitTier": oauth.get("rateLimitTier"),
        }
    });
    let body = serde_json::to_string_pretty(&placeholder)?;
    atomic_write(&claude_dir.join(".credentials.json"), body.as_bytes(), 0o600)?;

    Ok(Some(token_file))
}

fn refresh_openai(state_dir: &Path) -> Result<Option<PathBuf>> {
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
    let token_file = openai_token_path(state_dir);
    atomic_write(&token_file, access_token.as_bytes(), 0o600)?;

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
                Value::String("MSB_PLACEHOLDER_OPENAI_REFRESH_TOKEN_v1".to_string()),
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

    Ok(Some(token_file))
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
