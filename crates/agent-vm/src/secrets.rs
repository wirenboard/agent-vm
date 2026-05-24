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
/// Synthetic JWT (alg:none) carrying only placeholder fields. Codex
/// parses `tokens.id_token` client-side at startup, so the placeholder
/// has to be structurally a JWT or codex refuses to load — but it has
/// no real PII. `email`, `chatgpt_account_id`, `chatgpt_plan_type`,
/// `chatgpt_subscription_active_until`, `chatgpt_user_id` are the
/// fields codex reads from the payload; values here are clearly-fake
/// so they're obvious in logs.
///
/// header  = base64url('{"alg":"none","typ":"JWT"}')
/// payload = base64url('{"email":"placeholder@msb.local","exp":9999999999,"iat":1700000000,
///                       "https://api.openai.com/auth":{"chatgpt_account_id":"00000000-0000-0000-0000-000000000000",
///                       "chatgpt_plan_type":"placeholder","chatgpt_subscription_active_until":"9999-12-31T00:00:00+00:00",
///                       "chatgpt_user_id":"user-placeholder"},"sub":"placeholder|0"}')
/// sig     = "MSB_PLACEHOLDER_OPENAI_ID_TOKEN_v1"  (kept literal so a
///           string grep for the v1 marker still flags any place this
///           leaks; the JWT spec allows arbitrary characters in the
///           signature segment under alg:none)
pub const OPENAI_ID_PLACEHOLDER: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJlbWFpbCI6InBsYWNlaG9sZGVyQG1zYi5sb2NhbCIsImV4cCI6OTk5OTk5OTk5OSwiaWF0IjoxNzAwMDAwMDAwLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiMDAwMDAwMDAtMDAwMC0wMDAwLTAwMDAtMDAwMDAwMDAwMDAwIiwiY2hhdGdwdF9wbGFuX3R5cGUiOiJwbGFjZWhvbGRlciIsImNoYXRncHRfc3Vic2NyaXB0aW9uX2FjdGl2ZV91bnRpbCI6Ijk5OTktMTItMzFUMDA6MDA6MDArMDA6MDAiLCJjaGF0Z3B0X3VzZXJfaWQiOiJ1c2VyLXBsYWNlaG9sZGVyIn0sInN1YiI6InBsYWNlaG9sZGVyfDAifQ.MSB_PLACEHOLDER_OPENAI_ID_TOKEN_v1";

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

/// Host-only directory holding the real access-token files the proxy
/// re-reads via `SecretValue::File`.
///
/// **Must live outside `state_dir`.** The launcher bind-mounts
/// `state_dir` into the guest at `/agent-vm-state` (a single mount, to
/// stay under libkrun's virtio-IRQ cap), so anything written *under*
/// `state_dir` is readable from inside the VM — a `cat
/// /agent-vm-state/tokens/anthropic` would hand the in-VM agent the
/// host's real token and defeat the entire point of Phase 3/4. The
/// microsandbox proxy reads these files on the *host* side, so they
/// never need to be mounted; we keep them in a sibling `<hash>.secrets/`
/// directory that is never bind-mounted anywhere.
fn host_secret_dir(state_dir: &Path) -> PathBuf {
    let name = state_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    let parent = state_dir.parent().unwrap_or(state_dir);
    parent.join(format!("{name}.secrets"))
}

/// Per-project location of the token file the proxy re-reads. Lives in
/// the host-only [`host_secret_dir`], never inside the guest mount.
pub fn anthropic_token_path(state_dir: &Path) -> PathBuf {
    host_secret_dir(state_dir).join("anthropic")
}

pub fn openai_token_path(state_dir: &Path) -> PathBuf {
    host_secret_dir(state_dir).join("openai")
}

/// Read host credentials, write the token file (atomically, 0600) and
/// the guest-side placeholder credentials.json. Returns the paths to
/// the written token files so the launcher can plumb them into
/// microsandbox's SecretValue::File config.
pub fn refresh(state_dir: &Path, project_guest_path: &str) -> Result<CredsState> {
    // The token files hold the host's *real* access tokens, so their
    // directory must never be bind-mounted into the guest. Create it
    // 0700 in the host-only sibling location (see `host_secret_dir`).
    let secret_dir = host_secret_dir(state_dir);
    std::fs::create_dir_all(&secret_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secret_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 700 {}", secret_dir.display()))?;
    }
    // Best-effort: an older agent-vm wrote token files to
    // `state_dir/tokens/`, which is inside the guest bind mount. If a
    // user upgrades over such a state dir, remove the stale dir so a real
    // token doesn't linger where the guest can read it.
    let _ = std::fs::remove_dir_all(state_dir.join("tokens"));

    // First-run bypasses, run regardless of whether the user has host
    // credentials for the provider. Without these the in-VM agent
    // blocks on a terminal-style wizard at first launch.
    write_agent_config_defaults(state_dir, project_guest_path)?;

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

/// Drop the per-agent bypass files (Claude's onboarding flags + Codex's
/// trust/approval settings) into the per-project state dir. Idempotent
/// across launches; merges instead of overwrites so user tweaks
/// survive.
fn write_agent_config_defaults(state_dir: &Path, project_guest_path: &str) -> Result<()> {
    let claude_dir = state_dir.join("claude");
    std::fs::create_dir_all(&claude_dir)?;
    write_default_claude_settings(&claude_dir.join("settings.json"))?;
    // ~/.claude.json is the per-user onboarding-state file Claude
    // Code checks for the "first launch" theme picker AND the
    // per-project "trust this folder?" prompt. It sits at $HOME root
    // (not inside .claude/), so the symlinked state dir doesn't catch
    // it — we instead persist it in our state dir and run.rs symlinks
    // /root/.claude.json → /agent-vm-state/claude.json.
    write_default_claude_root_state(&state_dir.join("claude.json"), project_guest_path)?;

    let codex_dir = state_dir.join("codex");
    std::fs::create_dir_all(&codex_dir)?;
    write_default_codex_config(&codex_dir.join("config.toml"))?;

    Ok(())
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

fn write_default_claude_settings(path: &Path) -> Result<()> {
    // Read what's there (if anything), force-set the onboarding-bypass
    // fields, write back. Merging instead of overwriting means a user
    // who tweaked some other setting inside the sandbox keeps their
    // change; force-setting `hasCompletedOnboarding` covers the case
    // where Claude wrote a partial settings.json mid-wizard.
    let mut settings: Value = match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or(serde_json::json!({})),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let obj = settings.as_object_mut().context("settings.json is not an object")?;
    obj.entry("theme".to_string())
        .or_insert(Value::String("dark".into()));
    obj.insert("hasCompletedOnboarding".into(), Value::Bool(true));
    obj.insert("skipDangerousModePermissionPrompt".into(), Value::Bool(true));
    obj.entry("effortLevel".to_string())
        .or_insert(Value::String("xhigh".into()));
    atomic_write(path, serde_json::to_vec(&settings)?.as_slice(), 0o644)?;
    Ok(())
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
        // The ChatGPT auth flow also stores an `id_token` JWT — it
        // carries the user's email, org list, plan type, etc. and is
        // itself a credential at OIDC-protected endpoints. Leaving it
        // verbatim would leak that into the guest's auth.json; the
        // refresh hook already uses OPENAI_ID_PLACEHOLDER for the
        // synthesized response, so do the same on initial snapshot.
        if tokens.contains_key("id_token") {
            tokens.insert(
                "id_token".into(),
                Value::String(OPENAI_ID_PLACEHOLDER.into()),
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

fn write_default_claude_root_state(path: &Path, project_guest_path: &str) -> Result<()> {
    // Merge-on-existing to preserve user-side updates (project entries
    // etc.) but force-set the onboarding + per-folder trust flags
    // every launch.
    let mut state: Value = match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or(serde_json::json!({})),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let obj = state.as_object_mut().context("~/.claude.json is not an object")?;
    obj.insert("hasCompletedOnboarding".into(), Value::Bool(true));
    obj.insert("bypassPermissionsModeAccepted".into(), Value::Bool(true));

    // Pre-approve the project folder. Without this the in-VM Claude
    // shows the "do you trust the files in this folder?" prompt on
    // first launch in each new project. The keys here mirror what
    // Claude Code itself writes once a user clicks "yes".
    let projects = obj
        .entry("projects".to_string())
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("~/.claude.json projects is not an object")?;
    let project = projects
        .entry(project_guest_path.to_string())
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("~/.claude.json projects.<path> is not an object")?;
    project.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
    project.insert(
        "hasCompletedProjectOnboarding".into(),
        Value::Bool(true),
    );
    project
        .entry("history".to_string())
        .or_insert_with(|| serde_json::json!([]));

    atomic_write(path, serde_json::to_vec(&state)?.as_slice(), 0o644)?;
    Ok(())
}

fn write_default_codex_config(path: &Path) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut f = match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o644)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let body = "sandbox_mode = \"danger-full-access\"\n\
                approval_policy = \"never\"\n";
    f.write_all(body.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Security invariant: the real-token files must never live under
    /// `state_dir`, because the launcher bind-mounts `state_dir` into the
    /// guest at `/agent-vm-state`. A token file under that path would be
    /// readable by the in-VM agent (`cat /agent-vm-state/tokens/...`),
    /// defeating the whole "real tokens never enter the VM" design.
    #[test]
    fn token_files_live_outside_the_guest_mount() {
        let state_dir = Path::new("/home/u/.local/state/agent-vm/abc123");
        for token in [
            anthropic_token_path(state_dir),
            openai_token_path(state_dir),
        ] {
            assert!(
                !token.starts_with(state_dir),
                "{} must not be under the bind-mounted state dir {}",
                token.display(),
                state_dir.display(),
            );
            // ...but still derivable from it (same parent) so the launcher
            // and the refresh hook agree on the path.
            assert_eq!(token.parent().unwrap().parent(), state_dir.parent());
        }
    }
}
