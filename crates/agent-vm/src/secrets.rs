//! Host-rooted credentials.
//!
//! At launch we snapshot the host's Claude / Codex credential files,
//! write placeholder credentials into the guest-side state directory,
//! and return the per-project token-file *paths* to the launcher. The
//! launcher registers them as microsandbox `SecretValue::File`
//! entries; the patched msb re-reads the file on every TLS-intercepted
//! connection so a host-side rotation is picked up on the next request.
//!
//! The same token files are rewritten by `intercept_hook` when the
//! in-VM agent's OAuth refresh attempt fires.
//!
//! Placeholders are stable per-version so credentials JSON written by
//! a prior invocation is still valid for the current one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::host_paths::{atomic_write, host_claude_creds_path, host_codex_auth_path};

// ---------------------------------------------------------------------------
// Placeholder strings the guest sees instead of real tokens. Substituted
// for the real value at the network layer on the way out, and forged
// into OAuth refresh responses by `intercept_hook`.

pub const ANTHROPIC_ACCESS_PLACEHOLDER: &str = "MSB_PLACEHOLDER_ANTHROPIC_ACCESS_TOKEN_v1";
pub const ANTHROPIC_REFRESH_PLACEHOLDER: &str = "MSB_PLACEHOLDER_ANTHROPIC_REFRESH_TOKEN_v1";
pub const OPENAI_ACCESS_PLACEHOLDER: &str = "MSB_PLACEHOLDER_OPENAI_ACCESS_TOKEN_v1";
pub const OPENAI_REFRESH_PLACEHOLDER: &str = "MSB_PLACEHOLDER_OPENAI_REFRESH_TOKEN_v1";
pub const OPENAI_ID_PLACEHOLDER: &str = "MSB_PLACEHOLDER_OPENAI_ID_TOKEN_v1";

// Hostnames the secret-substitution proxy + interceptor key off. Kept
// here so the launcher (`run.rs`), the hook (`intercept_hook`), and any
// docs stay in lockstep.

pub const ANTHROPIC_API_HOST: &str = "api.anthropic.com";
pub const ANTHROPIC_OAUTH_HOST: &str = "platform.claude.com";
pub const OPENAI_API_HOST: &str = "api.openai.com";
pub const OPENAI_CHATGPT_HOST: &str = "chatgpt.com";
pub const OPENAI_OAUTH_HOST: &str = "auth.openai.com";

pub const ANTHROPIC_OAUTH_TOKEN_PATH: &str = "/v1/oauth/token";
pub const OPENAI_OAUTH_TOKEN_PATH: &str = "/oauth/token";

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
    std::fs::create_dir_all(state_dir.join("tokens"))?;

    let anthropic_token_file = refresh_anthropic(state_dir).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "anthropic credential refresh failed; skipping");
        None
    });
    let openai_token_file = refresh_openai(state_dir).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "openai credential refresh failed; skipping");
        None
    });

    Ok(CredsState {
        anthropic_token_file,
        openai_token_file,
    })
}

fn refresh_anthropic(state_dir: &Path) -> Result<Option<PathBuf>> {
    let Some(host_path) = host_claude_creds_path() else {
        return Ok(None);
    };
    let raw = match std::fs::read_to_string(&host_path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", host_path.display())),
    };
    let json: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", host_path.display()))?;

    let oauth = json
        .get("claudeAiOauth")
        .context("host .credentials.json missing claudeAiOauth")?;
    let access_token = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())
        .context("host claudeAiOauth missing accessToken")?;

    let token_file = anthropic_token_path(state_dir);
    atomic_write(&token_file, access_token.as_bytes(), 0o600)?;

    let claude_dir = state_dir.join("claude");
    std::fs::create_dir_all(&claude_dir)?;
    let placeholder = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": ANTHROPIC_ACCESS_PLACEHOLDER,
            "refreshToken": ANTHROPIC_REFRESH_PLACEHOLDER,
            "expiresAt": oauth.get("expiresAt"),
            "scopes": oauth.get("scopes"),
            "subscriptionType": oauth.get("subscriptionType"),
            "rateLimitTier": oauth.get("rateLimitTier"),
        }
    });
    atomic_write(
        &claude_dir.join(".credentials.json"),
        serde_json::to_vec(&placeholder)?.as_slice(),
        0o600,
    )?;

    Ok(Some(token_file))
}

fn refresh_openai(state_dir: &Path) -> Result<Option<PathBuf>> {
    let Some(host_path) = host_codex_auth_path() else {
        return Ok(None);
    };
    let raw = match std::fs::read_to_string(&host_path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", host_path.display())),
    };
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
                Value::String(OPENAI_ACCESS_PLACEHOLDER.into()),
            );
        }
        if tokens.contains_key("refresh_token") {
            tokens.insert(
                "refresh_token".into(),
                Value::String(OPENAI_REFRESH_PLACEHOLDER.into()),
            );
        }
    }
    if json.get("OPENAI_API_KEY").is_some() {
        json["OPENAI_API_KEY"] = Value::String(OPENAI_ACCESS_PLACEHOLDER.into());
    }

    let codex_dir = state_dir.join("codex");
    std::fs::create_dir_all(&codex_dir)?;
    atomic_write(
        &codex_dir.join("auth.json"),
        serde_json::to_vec(&json)?.as_slice(),
        0o600,
    )?;

    Ok(Some(token_file))
}
