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
use sha2::{Digest, Sha256};

use crate::host_paths::{
    atomic_write, host_claude_creds_path, host_codex_auth_path, host_opencode_auth_path,
};

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
/// Synthetic JWT used as the placeholder for OpenCode's OAuth `access`
/// field. OpenCode sends `Authorization: Bearer <access>` to
/// api.openai.com, so this string is the exact byte sequence the proxy
/// scans for and substitutes with the real OpenAI access token (kept
/// in the same host-only token file as Codex uses). Must be distinct
/// from `OPENAI_ID_PLACEHOLDER` so that substituting one doesn't
/// accidentally substitute the other in unrelated request bytes.
///
/// header  = base64url('{"alg":"none","typ":"JWT"}')
/// payload = base64url('{"exp":9999999999,
///                       "chatgpt_account_id":"00000000-0000-0000-0000-000000000000"}')
/// sig     = "MSB_OPENCODE_v1"
///
/// **Kept short on purpose:** an earlier ~480-char payload (with
/// iss/aud/scp/email/sub claims) triggered upstream issue #8 — long
/// placeholders fail sandbox boot with `handshake read id_offset:
/// timed out before relay sent bytes`. Add fields here only if
/// OpenCode actually parses them and chokes on absence.
pub const OPENCODE_OPENAI_ACCESS_PLACEHOLDER: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJleHAiOjk5OTk5OTk5OTksImNoYXRncHRfYWNjb3VudF9pZCI6IjAwMDAwMDAwLTAwMDAtMDAwMC0wMDAwLTAwMDAwMDAwMDAwMCJ9.MSB_OPENCODE_v1";
pub const OPENCODE_OPENAI_REFRESH_PLACEHOLDER: &str = "MSB_PLACEHOLDER_OPENCODE_OPENAI_REFRESH_v1";
/// Placeholder for the host's `gh auth token`. The in-guest `gh` /
/// git credential helper sees this string; the proxy substitutes the
/// real bearer on outbound traffic to GitHub.
pub const GH_TOKEN_PLACEHOLDER: &str = "MSB_PLACEHOLDER_GH_TOKEN_v1";

// Hostnames the secret-substitution proxy + interceptor key off. Kept
// here so the launcher (`run.rs`), the hook (`intercept_hook`), and any
// docs stay in lockstep.

pub const ANTHROPIC_API_HOST: &str = "api.anthropic.com";
pub const ANTHROPIC_OAUTH_HOST: &str = "platform.claude.com";
/// Claude Code's MCP relay endpoint. Claude Code's HTTP client sends
/// the same Anthropic access token here, so the secret substitution
/// has to allow this destination too — otherwise the placeholder
/// trips the violation scan and the conn gets dropped, breaking MCP.
pub const ANTHROPIC_MCP_PROXY_HOST: &str = "mcp-proxy.anthropic.com";
pub const OPENAI_API_HOST: &str = "api.openai.com";
pub const OPENAI_CHATGPT_HOST: &str = "chatgpt.com";
pub const OPENAI_OAUTH_HOST: &str = "auth.openai.com";

pub const GITHUB_API_HOST: &str = "api.github.com";
pub const GITHUB_HOST: &str = "github.com";
pub const GITHUB_CODELOAD_HOST: &str = "codeload.github.com";
pub const GITHUB_RAW_HOST: &str = "raw.githubusercontent.com";
pub const GITHUB_OBJECTS_HOST: &str = "objects.githubusercontent.com";

pub const ANTHROPIC_OAUTH_TOKEN_PATH: &str = "/v1/oauth/token";
pub const OPENAI_OAUTH_TOKEN_PATH: &str = "/oauth/token";

/// Result of [`refresh`]. `*_token_file` paths only exist if the host
/// credential file was found and parsed successfully.
///
/// `opencode_openai_access_token_file` shares the same on-disk file as
/// `openai_token_file` (both substitute to the same real OpenAI access
/// token) — it's `Some` whenever the launcher should register a
/// proxy-substitution entry for OpenCode's synthetic-JWT placeholder.
#[derive(Debug, Default, Clone)]
pub struct CredsState {
    pub anthropic_token_file: Option<PathBuf>,
    pub openai_token_file: Option<PathBuf>,
    pub opencode_openai_access_token_file: Option<PathBuf>,
    /// File holding the host's `gh auth token` (a GitHub user OAuth
    /// token). The proxy substitutes `GH_TOKEN_PLACEHOLDER` for this
    /// on outbound traffic to GitHub. Only `Some` when the user has
    /// `gh` logged in *and* `--no-git` was not passed.
    pub gh_token_file: Option<PathBuf>,
    pub snapshot: Option<HostCredsSnapshot>,
}

/// SHA-256 of each host credential file at launcher start. Compared
/// after the sandbox exits to flag unexpected mutations — the Phase 4
/// refresh hook may legitimately rewrite these files; anything else
/// touching them is a smell. See `verify_snapshot`.
#[derive(Debug, Default, Clone)]
pub struct HostCredsSnapshot {
    pub claude: Option<(PathBuf, String)>,
    pub codex: Option<(PathBuf, String)>,
    pub opencode: Option<(PathBuf, String)>,
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

pub fn gh_token_path(state_dir: &Path) -> PathBuf {
    host_secret_dir(state_dir).join("gh")
}

/// OpenCode reuses the same OpenAI access token file: both Codex and
/// OpenCode hit api.openai.com / chatgpt.com and the proxy substitutes
/// each provider's distinct placeholder string for the same real
/// bearer. Same file, two registered placeholders.
pub fn opencode_openai_token_path(state_dir: &Path) -> PathBuf {
    openai_token_path(state_dir)
}

/// Read host credentials, write the token file (atomically, 0600) and
/// the guest-side placeholder credentials.json. Returns the paths to
/// the written token files so the launcher can plumb them into
/// microsandbox's SecretValue::File config.
pub fn refresh(
    state_dir: &Path,
    project_guest_path: &str,
    use_github: bool,
) -> Result<CredsState> {
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
    // OpenCode auths against OpenAI like Codex does. If the user has
    // host Codex/OpenAI credentials, we synthesize an OpenCode-shaped
    // `auth.json` whose `access` field is a placeholder JWT — the
    // proxy substitutes that placeholder for the same real OpenAI
    // access token on outbound traffic. So OpenCode shares the
    // `openai_token_file` with Codex.
    let opencode_openai_access_token_file = if openai_token_file.is_some() {
        match refresh_opencode(state_dir) {
            // Only register the secret when refresh_opencode actually
            // wrote a placeholder auth.json. `Ok(None)` (host file
            // missing) means we have nothing to wire up — review
            // finding #13.
            Ok(Some(())) => Some(opencode_openai_token_path(state_dir)),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(error = %e, "opencode credential refresh failed; skipping");
                None
            }
        }
    } else {
        None
    };

    // Phase 6: capture the user's `gh auth token` (if any and not
    // suppressed via `--no-git`). The launcher passes
    // `--no-git`/use_github=false when the user opted out or when no
    // GitHub remote was found and no `--repo` overrides were given.
    let gh_token_file = if use_github {
        refresh_gh(state_dir).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "gh credential capture failed; skipping");
            None
        })
    } else {
        None
    };

    // SHA-256 snapshot of host credential files for post-run mutation
    // detection. Phase 4's refresh hook *legitimately* rewrites these;
    // anything else doing so is a bug to investigate. See
    // `verify_snapshot`.
    let snapshot = Some(snapshot_host_creds());

    Ok(CredsState {
        anthropic_token_file,
        openai_token_file,
        opencode_openai_access_token_file,
        gh_token_file,
        snapshot,
    })
}

/// Capture `gh auth token` from the host into a 0600 file under
/// `<state>.secrets/gh`. Returns `None` if `gh` isn't installed or the
/// user isn't logged in. The proxy substitutes `GH_TOKEN_PLACEHOLDER`
/// for this file's content on outbound GitHub traffic.
fn refresh_gh(state_dir: &Path) -> Result<Option<PathBuf>> {
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();
    let out = match out {
        Ok(o) => o,
        // gh not on PATH — fine, just skip
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("running `gh auth token`"),
    };
    if !out.status.success() {
        // Most likely "not logged in" — non-fatal.
        return Ok(None);
    }
    let token = String::from_utf8(out.stdout)
        .context("`gh auth token` output is not UTF-8")?;
    let token = token.trim();
    if token.is_empty() {
        return Ok(None);
    }
    let token_file = gh_token_path(state_dir);
    atomic_write(&token_file, token.as_bytes(), 0o600)?;
    Ok(Some(token_file))
}

/// SHA-256 the three host credential files. Files that don't exist or
/// can't be read are recorded as `None` — only files that successfully
/// hash become anchors for [`verify_snapshot`].
pub fn snapshot_host_creds() -> HostCredsSnapshot {
    HostCredsSnapshot {
        claude: host_claude_creds_path().and_then(|p| hash_file(&p).map(|h| (p, h))),
        codex: host_codex_auth_path().and_then(|p| hash_file(&p).map(|h| (p, h))),
        opencode: host_opencode_auth_path().and_then(|p| hash_file(&p).map(|h| (p, h))),
    }
}

/// Diff the saved [`HostCredsSnapshot`] against the current file
/// state. Emits a one-line summary to stderr listing the host cred
/// files that mutated during the session — the Phase 4 OAuth refresh
/// hook may legitimately rewrite them, but any other mutation is
/// worth surfacing. Non-fatal.
pub fn verify_snapshot(before: &HostCredsSnapshot) {
    let mut changed: Vec<&str> = Vec::new();
    for (label, entry) in [
        ("claude", &before.claude),
        ("codex", &before.codex),
        ("opencode", &before.opencode),
    ] {
        if let Some((path, before_hash)) = entry {
            let now = hash_file(path);
            match now.as_deref() {
                Some(after) if after == before_hash => {}
                Some(_) => changed.push(label),
                None => changed.push(label), // disappeared
            }
        }
    }
    if !changed.is_empty() {
        eprintln!(
            "==> host credential file(s) changed during sandbox: {} (expected only on Phase 4 OAuth refresh; investigate if you didn't trigger one)",
            changed.join(", "),
        );
    }
}

fn hash_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let mut h = Sha256::new();
    h.update(&bytes);
    let digest = h.finalize();
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
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

    // OpenCode reads its config from ~/.config/opencode/opencode.json
    // (XDG config dir, file named opencode.json — NOT the data dir
    // and NOT config.json). The launcher symlinks
    // /root/.config/opencode → /agent-vm-state/opencode-config so
    // this file lands at the right path inside the guest. Without an
    // explicit `model`, OpenCode defaults to `openai/gpt-5.5-pro`,
    // which OpenAI rejects for ChatGPT-OAuth accounts with "model
    // not supported when using Codex with a ChatGPT account." Pin a
    // ChatGPT-supported model as the default; users can override
    // per-run via `opencode run --model ...`.
    let opencode_config_dir = state_dir.join("opencode-config");
    std::fs::create_dir_all(&opencode_config_dir)?;
    write_default_opencode_config(&opencode_config_dir.join("opencode.json"))?;

    // Persistent per-project bash history. The launcher symlinks
    // /root/.bash_history → /agent-vm-state/bash_history; touching
    // the file here ensures the symlink target exists on first
    // launch (bash refuses to write history if the target's parent
    // dir is missing — by here state_dir exists, so just ensure
    // the file does too). Subsequent launches preserve whatever
    // bash appended on exit.
    let history_path = state_dir.join("bash_history");
    if !history_path.exists() {
        atomic_write(&history_path, b"", 0o600)?;
    }

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

/// Write `<state>/opencode/auth.json` shaped for OpenCode's OAuth
/// flow, but with placeholder strings everywhere. The `openai.access`
/// field carries our synthetic JWT placeholder; the proxy substitutes
/// it with the real OpenAI access token (from the file shared with
/// Codex) on outbound traffic. `accountId` is derived from the host
/// Codex JWT when available, so OpenCode picks the right account
/// without us hard-coding anything user-specific.
///
/// Requires that `refresh_openai` has already run (so a host codex
/// auth file existed and was parseable). Returns `None` if not.
fn refresh_opencode(state_dir: &Path) -> Result<Option<()>> {
    let Some(host_path) = host_codex_auth_path() else {
        return Ok(None);
    };
    let raw = match std::fs::read_to_string(&host_path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", host_path.display())),
    };
    let json: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", host_path.display()))?;

    // account_id: from tokens.account_id directly when present,
    // otherwise pull from the id_token JWT's `chatgpt_account_id`.
    let account_id = json
        .pointer("/tokens/account_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| decode_id_token_account(&json))
        .unwrap_or_else(|| "00000000-0000-0000-0000-000000000000".into());

    // Far-future expires_in (ms); OpenCode treats this as a hint of
    // when to refresh. The proxy substitution is always live anyway,
    // so a long expiry just suppresses opencode's own refresh
    // attempts (which would fail against our synthetic JWT).
    let expires_ms: u64 = 9_999_999_999_000;

    let auth = serde_json::json!({
        "openai": {
            "type": "oauth",
            "refresh": OPENCODE_OPENAI_REFRESH_PLACEHOLDER,
            "access": OPENCODE_OPENAI_ACCESS_PLACEHOLDER,
            "expires": expires_ms,
            "accountId": account_id,
        }
    });

    let opencode_dir = state_dir.join("opencode");
    std::fs::create_dir_all(&opencode_dir)?;
    atomic_write(
        &opencode_dir.join("auth.json"),
        serde_json::to_vec(&auth)?.as_slice(),
        0o600,
    )?;

    Ok(Some(()))
}

/// Decode an OpenAI id_token JWT (alg=RS256, but we don't verify) and
/// pull the `chatgpt_account_id` out of its payload's
/// `https://api.openai.com/auth` claim. Used as a fallback when the
/// codex auth.json doesn't carry `tokens.account_id` directly.
fn decode_id_token_account(json: &Value) -> Option<String> {
    let id_token = json
        .pointer("/tokens/id_token")
        .and_then(|v| v.as_str())?;
    let payload_b64 = id_token.split('.').nth(1)?;
    use base64::Engine as _;
    // JWT spec says base64url *without* padding. Use URL_SAFE_NO_PAD
    // first (the conformant variant); fall back to STANDARD (some
    // libraries emit JWTs with '+'/'/' instead of '-'/'_') — review
    // finding #14.
    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64.as_bytes())
        .or_else(|_| {
            let padded = format!(
                "{}{}",
                payload_b64.replace('-', "+").replace('_', "/"),
                "=".repeat((4 - payload_b64.len() % 4) % 4)
            );
            base64::engine::general_purpose::STANDARD.decode(padded.as_bytes())
        })
        .ok()?;
    let payload: Value = serde_json::from_slice(&payload_bytes).ok()?;
    payload
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .and_then(|v| v.as_str())
        .map(String::from)
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

    // Phase 7: Chrome DevTools MCP server. Force-set this entry so
    // the in-VM Claude can drive a real headless Chromium for tasks
    // that need browser interaction. The user-set `mcpServers` map
    // is preserved otherwise. Opt out via AGENT_VM_NO_CHROME_MCP=1.
    if std::env::var("AGENT_VM_NO_CHROME_MCP").is_err() {
        let mcp = obj
            .entry("mcpServers".to_string())
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .context("~/.claude.json mcpServers is not an object")?;
        mcp.insert(
            "chrome-devtools".into(),
            serde_json::json!({
                "command": "npx",
                "args": ["-y", "chrome-devtools-mcp@latest", "--headless=true", "--isolated=true"],
            }),
        );
    }

    atomic_write(path, serde_json::to_vec(&state)?.as_slice(), 0o644)?;
    Ok(())
}

/// Phase 6/9: write the guest's gitconfig and (if `has_gh_token`)
/// gh/git credential plumbing. Always called so the
/// `safe.directory = *` line is in place — without it, git inside the
/// guest fails with "fatal: detected dubious ownership in repository"
/// because the host-owned bind-mounted project is read by the guest's
/// root user (different UID).
///
/// Files land under `state_dir` so they're available inside the guest
/// via the existing bind mount + symlinks (see run.rs patch builder).
///
/// - `<state>/gitconfig` → symlinked to `/root/.gitconfig` in the
///   guest. Always contains `safe.directory = *`. When the host has
///   gh auth, also contains a `credential.helper` that echoes
///   `username=x-access-token` / `password=<placeholder>` so
///   `git push` to GitHub goes out as
///   `Authorization: Basic base64(x-access-token:placeholder)`, which
///   the proxy substitutes on the wire.
/// - `<state>/gh-config/hosts.yml` → symlinked to `/root/.config/gh`
///   in the guest. Only written when `has_gh_token`; the placeholder
///   is what gh CLI sends and the proxy substitutes outbound to
///   api.github.com.
pub fn write_guest_gh_config(state_dir: &Path, has_gh_token: bool) -> Result<()> {
    // safe.directory = * is unconditional: the guest IS the security
    // boundary (microVM), so trusting every path is fine and git
    // operating on the host-bind-mounted project requires it. Use
    // wildcard form so any --mount extra also works without
    // listing every path.
    let mut gitconfig = String::from(
        "[safe]\n\
         \tdirectory = *\n\
         [user]\n\
         \tname = agent-vm\n\
         \temail = agent-vm@msb.local\n",
    );
    if has_gh_token {
        // git's credential helper for github.com pushes/clones. The
        // helper is a shell snippet; git invokes it with `get` and
        // reads `username=`/`password=` lines from stdout.
        gitconfig.push_str(&format!(
            "[credential \"https://github.com\"]\n\
             \thelper = \"!f() {{ test \\\"$1\\\" = get && echo username=x-access-token && echo password={tok}; }}; f\"\n\
             [credential \"https://gist.github.com\"]\n\
             \thelper = \"!f() {{ test \\\"$1\\\" = get && echo username=x-access-token && echo password={tok}; }}; f\"\n\
             [url \"https://github.com/\"]\n\
             \tinsteadOf = git@github.com:\n",
            tok = GH_TOKEN_PLACEHOLDER,
        ));
    }
    atomic_write(&state_dir.join("gitconfig"), gitconfig.as_bytes(), 0o600)?;

    if has_gh_token {
        let gh_dir = state_dir.join("gh-config");
        std::fs::create_dir_all(&gh_dir)?;
        let hosts_yml = format!(
            "github.com:\n\
             \\x20\\x20user: agent-vm\n\
             \\x20\\x20oauth_token: {tok}\n\
             \\x20\\x20git_protocol: https\n",
            tok = GH_TOKEN_PLACEHOLDER,
        )
        .replace("\\x20", " ");
        atomic_write(&gh_dir.join("hosts.yml"), hosts_yml.as_bytes(), 0o600)?;
    }
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

/// Write a minimal OpenCode config that pins the default model to one
/// the ChatGPT-OAuth flow accepts. Merge-on-existing so the user's own
/// settings (model overrides, MCP servers, etc.) survive across
/// launches; only fields we manage are force-set.
fn write_default_opencode_config(path: &Path) -> Result<()> {
    let mut config: Value = match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or(serde_json::json!({})),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let obj = config
        .as_object_mut()
        .context("opencode config.json is not an object")?;
    obj.entry("$schema".to_string())
        .or_insert(Value::String("https://opencode.ai/config.json".into()));
    // ChatGPT-OAuth doesn't accept gpt-5.5-pro (OpenCode's built-in
    // default); pin a model that does. Use `entry().or_insert` so a
    // user who set `model` to something else doesn't get clobbered.
    obj.entry("model".to_string())
        .or_insert(Value::String("openai/gpt-5.5".into()));
    obj.entry("autoupdate".to_string())
        .or_insert(Value::Bool(false));
    atomic_write(path, serde_json::to_vec(&config)?.as_slice(), 0o644)?;
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
            gh_token_path(state_dir),
            opencode_openai_token_path(state_dir),
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

    /// Trailing-slash state_dir was flagged in code review as a
    /// possible edge case where the secrets dir might land inside the
    /// mount. Verify it doesn't.
    #[test]
    fn host_secret_dir_safe_for_trailing_slash_state_dir() {
        // Path::file_name() strips trailing slashes — verify the
        // sibling-secrets pattern still works.
        for sd in [
            "/home/u/.local/state/agent-vm/abc123",
            "/home/u/.local/state/agent-vm/abc123/",
            "/tmp/agent-vm-state",
        ] {
            let sdp = Path::new(sd);
            let secret = host_secret_dir(sdp);
            assert!(
                !secret.starts_with(sdp.canonicalize().unwrap_or(sdp.to_path_buf())),
                "{} must not be inside {}",
                secret.display(),
                sdp.display(),
            );
        }
    }

    /// **Placeholder distinctness**. If one placeholder were a
    /// substring of another, the secret-substitution proxy would
    /// swap the wrong token on outbound bytes — silently corrupting
    /// requests. Verify no placeholder is a substring of any other.
    #[test]
    fn placeholders_are_pairwise_distinct() {
        let all: &[(&str, &str)] = &[
            ("ANTHROPIC_ACCESS", ANTHROPIC_ACCESS_PLACEHOLDER),
            ("ANTHROPIC_REFRESH", ANTHROPIC_REFRESH_PLACEHOLDER),
            ("OPENAI_ACCESS", OPENAI_ACCESS_PLACEHOLDER),
            ("OPENAI_REFRESH", OPENAI_REFRESH_PLACEHOLDER),
            ("OPENAI_ID", OPENAI_ID_PLACEHOLDER),
            ("OPENCODE_ACCESS", OPENCODE_OPENAI_ACCESS_PLACEHOLDER),
            ("OPENCODE_REFRESH", OPENCODE_OPENAI_REFRESH_PLACEHOLDER),
            ("GH_TOKEN", GH_TOKEN_PLACEHOLDER),
        ];
        for (a_name, a) in all {
            for (b_name, b) in all {
                if a_name == b_name {
                    continue;
                }
                assert!(
                    !a.contains(b) && !b.contains(a),
                    "placeholder {a_name:?} ({a:?}) and {b_name:?} ({b:?}) overlap as substrings — substitution would swap the wrong token"
                );
            }
        }
    }

    // ── hash_file ─────────────────────────────────────────────────

    #[test]
    fn hash_file_returns_none_for_missing_path() {
        let missing = Path::new("/this/path/very/definitely/does/not/exist/anywhere");
        assert_eq!(hash_file(missing), None);
    }

    #[test]
    fn hash_file_is_deterministic_for_known_input() {
        use std::io::Write as _;
        let tmpdir = std::env::temp_dir().join(format!(
            "agent-vm-hash-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&tmpdir).unwrap();
        let path = tmpdir.join("known");
        let mut f = std::fs::File::create(&path).unwrap();
        // Capital T is the famous one (`d7a8fbb...`).
        f.write_all(b"The quick brown fox jumps over the lazy dog").unwrap();
        drop(f);
        let h = hash_file(&path).unwrap();
        assert_eq!(
            h,
            "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592"
        );
        std::fs::remove_dir_all(&tmpdir).ok();
    }

    // ── decode_id_token_account ───────────────────────────────────

    fn make_jwt(payload_json: &str, alphabet: base64::engine::general_purpose::GeneralPurpose) -> String {
        use base64::Engine as _;
        // Header doesn't matter; payload is what we test.
        let header = alphabet.encode(b"{\"alg\":\"none\",\"typ\":\"JWT\"}");
        let payload = alphabet.encode(payload_json.as_bytes());
        // Trim padding when present — JWTs are unpadded.
        let h = header.trim_end_matches('=');
        let p = payload.trim_end_matches('=');
        format!("{h}.{p}.sig")
    }

    #[test]
    fn decode_id_token_account_urlsafe_jwt() {
        let payload = r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"abc-123"}}"#;
        let jwt = make_jwt(payload, base64::engine::general_purpose::URL_SAFE_NO_PAD);
        let json = serde_json::json!({"tokens": {"id_token": jwt}});
        assert_eq!(
            decode_id_token_account(&json).as_deref(),
            Some("abc-123"),
        );
    }

    #[test]
    fn decode_id_token_account_standard_alphabet_falls_back() {
        // Some libraries emit JWTs with standard-alphabet base64
        // (`+`/`/`) instead of URL-safe (`-`/`_`). The decoder must
        // try STANDARD as a fallback. Construct a payload whose
        // base64 encoding includes a `+` or `/` — most easily by
        // embedding bytes that base64 to those chars.
        let payload = r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct?+/"}}"#;
        let jwt = make_jwt(payload, base64::engine::general_purpose::STANDARD);
        let json = serde_json::json!({"tokens": {"id_token": jwt}});
        assert_eq!(
            decode_id_token_account(&json).as_deref(),
            Some("acct?+/"),
        );
    }

    #[test]
    fn decode_id_token_account_returns_none_for_missing_fields() {
        // No tokens.id_token at all.
        assert_eq!(decode_id_token_account(&serde_json::json!({})), None);
        // tokens present but id_token missing.
        assert_eq!(
            decode_id_token_account(&serde_json::json!({"tokens": {}})),
            None
        );
        // id_token present but malformed (no `.`).
        assert_eq!(
            decode_id_token_account(&serde_json::json!({"tokens": {"id_token": "garbage"}})),
            None
        );
        // id_token decodes but the OpenAI-auth claim is missing.
        let payload = r#"{"something": "else"}"#;
        let jwt = make_jwt(payload, base64::engine::general_purpose::URL_SAFE_NO_PAD);
        let json = serde_json::json!({"tokens": {"id_token": jwt}});
        assert_eq!(decode_id_token_account(&json), None);
    }

    // ── write_default_opencode_config merge semantics ─────────────

    #[test]
    fn opencode_config_first_write_creates_with_defaults() {
        let tmpdir = std::env::temp_dir().join(format!(
            "agent-vm-oc-cfg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&tmpdir).unwrap();
        let path = tmpdir.join("opencode.json");

        write_default_opencode_config(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["model"], "openai/gpt-5.5");
        assert_eq!(v["$schema"], "https://opencode.ai/config.json");
        assert_eq!(v["autoupdate"], false);

        std::fs::remove_dir_all(&tmpdir).ok();
    }

    #[test]
    fn opencode_config_preserves_user_model_override() {
        // A user who sets model = "openai/gpt-5-turbo" must not have
        // it clobbered on a subsequent launch.
        let tmpdir = std::env::temp_dir().join(format!(
            "agent-vm-oc-cfg-merge-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&tmpdir).unwrap();
        let path = tmpdir.join("opencode.json");

        std::fs::write(
            &path,
            r#"{"model": "openai/gpt-5-turbo", "extra": "user-data"}"#,
        )
        .unwrap();

        write_default_opencode_config(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["model"], "openai/gpt-5-turbo", "user override survived");
        assert_eq!(v["extra"], "user-data", "user-set field preserved");
        // Our defaults still filled in where the user didn't set them.
        assert_eq!(v["$schema"], "https://opencode.ai/config.json");
        assert_eq!(v["autoupdate"], false);

        std::fs::remove_dir_all(&tmpdir).ok();
    }
}
