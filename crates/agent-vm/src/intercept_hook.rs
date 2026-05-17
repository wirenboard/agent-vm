//! `agent-vm _intercept-hook` — the subprocess microsandbox calls
//! when an in-VM OAuth refresh attempt matches an intercept rule.
//!
//! Lifecycle for one matched request:
//!
//! 1. msb forks this process, pipes the decrypted HTTP request bytes
//!    on stdin, sets `MSB_INTERCEPT_SNI` and related env vars.
//! 2. We figure out which provider the request is for (from the SNI),
//!    spawn the corresponding host CLI (`claude -p hi --model sonnet`
//!    or `codex exec --skip-git-repo-check 'Reply OK'`) so the
//!    host-side credential file gets rotated.
//! 3. We re-read the rotated host credential file and rewrite the
//!    per-project token file the proxy reads (so the next non-refresh
//!    request from the in-VM agent picks up the new bearer).
//! 4. We synthesize an OAuth refresh response — same shape the
//!    upstream server would return, but the body's `access_token`
//!    field is the *placeholder*. The in-VM agent updates its local
//!    credentials.json to that placeholder, and the next request goes
//!    through with the placeholder, which the proxy substitutes for
//!    the now-fresh real token.
//! 5. We write the response on stdout and exit 0.
//!
//! The whole point: the in-VM agent thinks it refreshed normally and
//! got a new bearer; in reality the host CLI did the refresh and we
//! lied about which token to use. The placeholder/real swap is what
//! keeps real tokens out of the VM.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use serde_json::{Value, json};

#[derive(ClapArgs)]
pub struct Args {
    /// Per-project state directory (same one used by the launcher).
    /// We need it to know where to read host credentials and where
    /// to write the freshly-rotated token file.
    #[arg(long)]
    state_dir: PathBuf,
}

pub fn run(args: Args) -> Result<()> {
    let sni = std::env::var("MSB_INTERCEPT_SNI")
        .context("MSB_INTERCEPT_SNI not set; this command is invoked by msb's interceptor")?;

    let mut request = Vec::new();
    std::io::stdin()
        .read_to_end(&mut request)
        .context("reading request from stdin")?;

    // Sanity check that this is what we expect.
    if !looks_like_oauth_refresh(&request) {
        // Forward an opaque server error so the in-VM agent at least
        // gets a comprehensible failure rather than a hang. We don't
        // try to proxy the real request — by the time msb spawned us,
        // it already committed to not connecting upstream.
        write_response(&error_response(
            500,
            "request did not look like an OAuth refresh; agent-vm hook punted",
        ))?;
        return Ok(());
    }

    let response = match sni.as_str() {
        "platform.claude.com" => refresh_anthropic(&args.state_dir)?,
        "auth.openai.com" => refresh_openai(&args.state_dir)?,
        other => error_response(
            500,
            &format!("agent-vm hook has no logic for SNI {other}"),
        ),
    };
    write_response(&response)?;
    Ok(())
}

fn write_response(bytes: &[u8]) -> Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(bytes).context("writing response to stdout")?;
    out.flush().ok();
    Ok(())
}

/// Trigger host-side Claude refresh, re-read the host file, rewrite
/// the per-project token file, and synthesize the OAuth refresh JSON
/// response that the in-VM Claude expects.
fn refresh_anthropic(state_dir: &Path) -> Result<Vec<u8>> {
    // Trigger refresh on the host.
    trigger_host_refresh("claude", &["-p", "hi", "--model", "sonnet"])?;

    // Re-read the (now-rotated) host file.
    let host_path = host_claude_creds_path()?;
    let raw = std::fs::read_to_string(&host_path)
        .with_context(|| format!("reading {}", host_path.display()))?;
    let json: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", host_path.display()))?;
    let oauth = json
        .get("claudeAiOauth")
        .context("rotated host .credentials.json missing claudeAiOauth")?;
    let new_access = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())
        .context("rotated host claudeAiOauth missing accessToken")?;
    let expires_at = oauth.get("expiresAt").cloned().unwrap_or(json!(0));

    // Update the per-project token file so the proxy's next
    // SecretValue::File read picks up the new bearer.
    let token_file = crate::secrets::anthropic_token_path(state_dir);
    if let Some(parent) = token_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&token_file, new_access.as_bytes(), 0o600)?;

    // Synthesize the OAuth refresh response. Claude's refresh endpoint
    // returns a body like:
    //   {"access_token":"...", "refresh_token":"...", "expires_in":...}
    // The in-VM Claude writes that into its credentials.json. We put
    // placeholders in both token fields so the next request goes
    // through the substitution path.
    let expires_in = derive_expires_in(&expires_at);
    let body = json!({
        "access_token": crate::secrets::ANTHROPIC_PLACEHOLDER,
        "refresh_token": "MSB_PLACEHOLDER_ANTHROPIC_REFRESH_TOKEN_v1",
        "expires_in": expires_in,
        "token_type": "Bearer",
        "scope": oauth.get("scopes").cloned().unwrap_or(json!([])),
    });
    Ok(http_200_json(&serde_json::to_vec(&body)?))
}

fn refresh_openai(state_dir: &Path) -> Result<Vec<u8>> {
    trigger_host_refresh(
        "codex",
        &[
            "exec",
            "--skip-git-repo-check",
            "--dangerously-bypass-approvals-and-sandbox",
            "Reply with OK",
        ],
    )?;

    let host_path = host_codex_auth_path()?;
    let raw = std::fs::read_to_string(&host_path)
        .with_context(|| format!("reading {}", host_path.display()))?;
    let json: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", host_path.display()))?;

    let new_access = json
        .pointer("/tokens/access_token")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("OPENAI_API_KEY").and_then(|v| v.as_str()))
        .context("rotated host codex auth missing tokens.access_token or OPENAI_API_KEY")?;

    let token_file = crate::secrets::openai_token_path(state_dir);
    if let Some(parent) = token_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&token_file, new_access.as_bytes(), 0o600)?;

    // OpenAI's refresh response shape varies; this is a minimal one
    // good enough for codex to write its auth.json.
    let body = json!({
        "access_token": crate::secrets::OPENAI_PLACEHOLDER,
        "refresh_token": "MSB_PLACEHOLDER_OPENAI_REFRESH_TOKEN_v1",
        "id_token": "MSB_PLACEHOLDER_OPENAI_ID_TOKEN_v1",
        "expires_in": 3600,
        "token_type": "Bearer",
    });
    Ok(http_200_json(&serde_json::to_vec(&body)?))
}

fn trigger_host_refresh(cmd: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .with_context(|| format!("spawning host {cmd}"))?;
    if !out.status.success() {
        bail!(
            "host {cmd} failed (status {}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn host_claude_creds_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".claude/.credentials.json"))
}

fn host_codex_auth_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".codex/auth.json"))
}

fn looks_like_oauth_refresh(req: &[u8]) -> bool {
    let s = match std::str::from_utf8(req) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let first_line = s.lines().next().unwrap_or("");
    first_line.starts_with("POST ")
}

fn derive_expires_in(expires_at_field: &Value) -> i64 {
    // claudeAiOauth.expiresAt is ms-since-epoch. We need seconds-until-expiry.
    let expires_at_ms = expires_at_field.as_i64().unwrap_or(0);
    if expires_at_ms == 0 {
        return 3600;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let diff_secs = (expires_at_ms - now_ms) / 1000;
    if diff_secs <= 0 { 3600 } else { diff_secs }
}

fn http_200_json(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 128);
    out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    out.extend_from_slice(b"Content-Type: application/json\r\n");
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"Connection: close\r\n");
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

fn error_response(code: u16, msg: &str) -> Vec<u8> {
    let body = format!("{{\"error\":{}}}", json!(msg));
    let mut out = Vec::with_capacity(body.len() + 128);
    out.extend_from_slice(format!("HTTP/1.1 {code} Server Error\r\n").as_bytes());
    out.extend_from_slice(b"Content-Type: application/json\r\n");
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"Connection: close\r\n");
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body.as_bytes());
    out
}

fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("agent-vm-hook-tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(data)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
