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

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use serde_json::{Value, json};

use crate::host_paths::{atomic_write, host_claude_creds_path, host_codex_auth_path};
use crate::secrets;

#[derive(ClapArgs)]
pub struct Args {
    /// Per-project state directory (same one used by the launcher).
    /// We need it to know where to write the freshly-rotated token file.
    #[arg(long)]
    state_dir: PathBuf,

    /// Repo allow-list for the GitHub forwarding path. Repeated:
    /// `--allowed-repo owner/name` (case-insensitive). Requests to
    /// `api.github.com` paths outside this list get a synthesized 403.
    /// Built from `git remote -v` in the cwd plus `--repo` overrides
    /// at launcher time.
    #[arg(long = "allowed-repo")]
    allowed_repos: Vec<String>,

    /// SNI of the intercepted connection. Provided by microsandbox via
    /// the `MSB_INTERCEPT_SNI` env var the proxy sets on the hook.
    #[arg(env = "MSB_INTERCEPT_SNI")]
    sni: String,
}

pub async fn run(args: Args) -> Result<()> {
    let mut request = Vec::new();
    std::io::stdin()
        .read_to_end(&mut request)
        .context("reading request from stdin")?;

    // GitHub gets its own dispatch — the request is forwarded upstream
    // after path-based allow-listing, not synthesized.
    if args.sni.eq_ignore_ascii_case(secrets::GITHUB_API_HOST) {
        let response = forward_github_api(&request, &args.allowed_repos, &args.state_dir)
            .await
            .unwrap_or_else(|e| {
                error_response(502, &format!("agent-vm github forwarder failed: {e}"))
            });
        write_response(&response)?;
        return Ok(());
    }

    // The git-smart-HTTP hosts (github.com, codeload, raw, objects)
    // are wired with `rule_streaming` upstream so the hook sees only
    // headers, not the (potentially MB-sized) pack body. We decide
    // based on the path alone: in-allow-list → empty stdout
    // (passthrough — proxy streams the rest to upstream with the
    // network secret layer substituting the placeholder bearer);
    // out-of-list → synthesized 403.
    let github_smart_hosts: [&str; 4] = [
        secrets::GITHUB_HOST,
        secrets::GITHUB_CODELOAD_HOST,
        secrets::GITHUB_RAW_HOST,
        secrets::GITHUB_OBJECTS_HOST,
    ];
    if github_smart_hosts
        .iter()
        .any(|h| args.sni.eq_ignore_ascii_case(h))
    {
        let response = match github_smart_decision(&request, &args.allowed_repos) {
            GithubSmartDecision::Passthrough => Vec::new(), // empty = passthrough
            GithubSmartDecision::Deny(msg) => error_response(403, &msg),
            GithubSmartDecision::Malformed => {
                error_response(400, "agent-vm github smart-HTTP filter: malformed request")
            }
        };
        write_response(&response)?;
        return Ok(());
    }

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

    let response = match args.sni.as_str() {
        secrets::ANTHROPIC_OAUTH_HOST => refresh_anthropic(&args.state_dir)?,
        secrets::OPENAI_OAUTH_HOST => refresh_openai(&args.state_dir)?,
        other => error_response(500, &format!("agent-vm hook has no logic for SNI {other}")),
    };
    write_response(&response)?;
    Ok(())
}

/// Forward an `api.github.com` request to the real upstream after
/// allow-list filtering. Workflow:
///
/// 1. Parse the buffered HTTP/1.1 request bytes (method + path +
///    headers + body).
/// 2. Extract the `owner/repo` slug from the path and check against
///    `allowed_repos`. Paths that don't fit the
///    `/repos/<owner>/<repo>/...` shape are still allowed if they're
///    user-scoped (`/user`, `/user/repos`) since gh CLI needs those
///    to function — those don't expose other-repo state.
/// 3. Read the real gh token from `<state>.secrets/gh` (written by
///    the launcher) and replace `GH_TOKEN_PLACEHOLDER` in the
///    `Authorization` header with it before forwarding.
/// 4. Make the upstream HTTPS request via `reqwest`, then format the
///    response as HTTP/1.1 bytes for the proxy to encrypt back to
///    the guest.
///
/// Bodies (request + response) are buffered in memory; OK for the gh
/// CLI / API use cases (JSON, tens of KB at most). Not suitable for
/// pack streams or large file uploads — those require streaming hook
/// support upstream (deferred).
async fn forward_github_api(
    request: &[u8],
    allowed_repos: &[String],
    state_dir: &Path,
) -> Result<Vec<u8>> {
    let (method, path, headers, body) = parse_http_request(request)
        .context("parsing intercepted github request")?;

    if !github_path_allowed(&method, &path, allowed_repos) {
        return Ok(error_response(
            403,
            &format!(
                "agent-vm: {method} {path:?} blocked by per-launch repo allow-list (read access is unrestricted; writes/mutations are scoped). Allowed repos: {}",
                if allowed_repos.is_empty() {
                    "(none — pass --repo OWNER/NAME or run inside a project with a github remote)".into()
                } else {
                    allowed_repos.join(", ")
                }
            ),
        ));
    }

    let real_token = read_gh_token(state_dir)
        .context("reading <state>.secrets/gh")?;

    let url = format!("https://{}{}", secrets::GITHUB_API_HOST, path);

    let client = reqwest::Client::builder()
        // Bounded upstream timeout so a hung api.github.com call
        // doesn't freeze the in-VM agent indefinitely (review #7).
        .timeout(std::time::Duration::from_secs(60))
        // Reflect 3xx back to the guest verbatim rather than
        // following — protects against unexpected redirect targets
        // and lets the agent decide (review #7).
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building reqwest client")?;
    let method_obj = reqwest::Method::from_bytes(method.as_bytes())
        .context("invalid HTTP method")?;
    let mut req = client.request(method_obj, &url);
    let mut had_authorization = false;
    for (name, value) in &headers {
        // Strip hop-by-hop + protocol-level headers; reqwest will
        // re-emit appropriate ones. `Host` is required to point at
        // api.github.com (overrides whatever the guest sent).
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "host" | "content-length" | "connection" | "transfer-encoding" | "te" | "keep-alive"
                | "proxy-authorization" | "proxy-authenticate" | "trailer" | "upgrade"
        ) {
            continue;
        }
        if lower == "authorization" {
            had_authorization = true;
            // Substitute the placeholder → real token. Two forms:
            //   - `token <PLACEHOLDER>` / `Bearer <PLACEHOLDER>` —
            //     literal substring, handled by `replace`.
            //   - `Basic base64(x-access-token:<PLACEHOLDER>)` —
            //     the placeholder is base64-encoded, so a literal
            //     replace finds nothing. Decode, substitute, re-
            //     encode. Review finding #12.
            let v = substitute_authorization_header(value, &real_token);
            req = req.header("Authorization", v);
        } else {
            req = req.header(name, value);
        }
    }
    // If the guest sent no Authorization header at all (some scripts
    // strip it before retry), inject a Bearer with the real token —
    // we've already checked the path against the allow-list, and the
    // alternative is silently anonymous traffic that gets a 401 and
    // confuses the agent. Review finding #11/#12.
    if !had_authorization {
        req = req.header("Authorization", format!("Bearer {real_token}"));
    }
    if !body.is_empty() {
        req = req.body(body);
    }

    let resp = req.send().await.context("upstream send to api.github.com")?;

    let status = resp.status();
    let mut out_headers: Vec<(String, String)> = Vec::new();
    for (k, v) in resp.headers() {
        let k_lower = k.as_str().to_ascii_lowercase();
        // Strip hop-by-hop headers (we set Content-Length below) AND
        // anything that lets the guest re-authenticate as the host
        // user without going through the substitution proxy. Review
        // finding #3: Set-Cookie + WWW-Authenticate would otherwise
        // let an in-VM agent harvest GitHub session cookies and
        // drive github.com directly.
        if matches!(
            k_lower.as_str(),
            "transfer-encoding"
                | "content-length"
                | "connection"
                | "keep-alive"
                | "set-cookie"
                | "set-cookie2"
                | "www-authenticate"
                | "proxy-authenticate"
        ) {
            continue;
        }
        out_headers.push((k.as_str().to_string(), v.to_str().unwrap_or("").to_string()));
    }
    let body_bytes = resp.bytes().await.context("reading upstream response body")?;

    let mut out = Vec::with_capacity(body_bytes.len() + 1024);
    let head = format!(
        "HTTP/1.1 {} {}\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
    out.extend_from_slice(head.as_bytes());
    for (k, v) in &out_headers {
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    out.extend_from_slice(format!("Content-Length: {}\r\n", body_bytes.len()).as_bytes());
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(&body_bytes);
    Ok(out)
}

/// Parse buffered HTTP/1.1 request bytes into (method, path, headers,
/// body). Headers are kept in original case for outbound. Best-effort
/// — assumes well-formed input from the in-guest CLI tool, errors
/// fail open to a 502 via the caller.
fn parse_http_request(req: &[u8]) -> Result<(String, String, Vec<(String, String)>, Vec<u8>)> {
    let hdr_end = req
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("no header/body separator")?;
    let header_block = std::str::from_utf8(&req[..hdr_end]).context("headers not UTF-8")?;
    let body = req[hdr_end + 4..].to_vec();
    let mut lines = header_block.split("\r\n");
    let request_line = lines.next().context("empty request")?;
    let mut parts = request_line.split_ascii_whitespace();
    let method = parts.next().context("no method")?.to_string();
    let path = parts.next().context("no path")?.to_string();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok((method, path, headers, body))
}

/// Method+path-based allow-list check for api.github.com requests.
///
/// **Policy:** read-only access is unrestricted (agents commonly need
/// to read arbitrary public/private GitHub state — browse a different
/// repo, look up an API, fetch a README, check who reviewed a PR).
/// Only **write / management** operations are scoped to the per-launch
/// allow-list, mirroring the user's actual concern: "don't let the
/// agent push to / mutate repos I didn't list."
///
/// Allowed unconditionally:
/// - Any `GET` or `HEAD` request (read-only).
/// - `POST /graphql` (gh PR/issue/etc. creation uses this). We do
///   **not** body-filter GraphQL mutations — that needs the request
///   body which the streaming intercept path doesn't see. Documented
///   gap: an agent that crafts a GraphQL mutation can do anything the
///   token can.
/// - `POST /markdown` (utility, no state change).
/// - `POST /user/repos`, `POST /repos/<owner>/<repo>/forks`,
///   `POST /repos/<owner>/<repo>/transfer` and similar repo-creation
///   shapes: these create new repos owned by the user — not "mutating
///   someone else's repo." Allowed.
///
/// Write methods on `/repos/<owner>/<repo>/*`:
/// - Allowed only when `<owner>/<repo>` (case-insensitive) is in
///   the allow-list. Catches PR creation, issue creation, comments,
///   merges, branch protection edits, releases, deletes — anything
///   that mutates a specific repo's state.
///
/// Traversal: any `..` path segment is rejected up front so a
/// crafted `/repos/<allowed>/<repo>/../../<victim>/<private>` can't
/// pass the prefix check and let GitHub resolve `..` upstream.
///
/// Anything else (write methods to paths outside /repos/ that aren't
/// in the explicit allow list above) → deny by default.
fn github_path_allowed(method: &str, path: &str, allowed: &[String]) -> bool {
    let p = path.split_once('?').map(|(p, _)| p).unwrap_or(path);

    // Reject `..` traversal anywhere. GitHub server-normalises `..`
    // and would otherwise resolve to a different repo than the one
    // we checked.
    for seg in p.split('/') {
        if seg == ".." {
            return false;
        }
    }

    // Reads — unrestricted.
    if matches!(method, "GET" | "HEAD") {
        return true;
    }

    // GraphQL: allow (no body-level filtering in v1).
    if p == "/graphql" {
        return true;
    }

    // Utility / write-but-not-other-repo-mutating endpoints.
    if matches!(p, "/markdown" | "/user/repos") {
        return true;
    }

    // Repo-scoped writes: only against the allow-list.
    if let Some(rest) = p.strip_prefix("/repos/") {
        let mut it = rest.split('/');
        let owner = it.next().unwrap_or("");
        let repo = it.next().unwrap_or("");
        if owner.is_empty() || repo.is_empty() {
            return false;
        }
        let slug = format!("{owner}/{repo}");
        return allowed.iter().any(|a| a.eq_ignore_ascii_case(&slug));
    }

    // Anything else (writes outside the explicit allow set above) is
    // denied. Reaching here means a write to e.g. /user/keys, /orgs/X
    // settings, /admin/* — none of which is a normal gh CLI flow.
    false
}

/// Decision for a request to a git-smart-HTTP host (github.com /
/// codeload / raw / objects).
enum GithubSmartDecision {
    /// Path matches the allow-list: tell the proxy to passthrough
    /// (empty hook stdout).
    Passthrough,
    /// Path is outside the allow-list: synthesize a 403 with `reason`.
    Deny(String),
    /// Couldn't parse the request well enough to decide; fall back to
    /// a 400. Rare, indicates a non-HTTP or truncated request.
    Malformed,
}

/// Check the first line of `request` against the per-launch repo
/// allow-list, applying the same read-unrestricted / writes-scoped
/// policy as the REST API: clone and fetch (`git-upload-pack`,
/// codeload archives, raw blobs) are allowed against any repo;
/// push (`git-receive-pack`) is scoped.
///
/// github.com smart-HTTP URLs (in scope for filtering):
///
///   GET  /<owner>/<repo>.git/info/refs?service=git-upload-pack   ← read
///   POST /<owner>/<repo>.git/git-upload-pack                      ← read
///   GET  /<owner>/<repo>.git/info/refs?service=git-receive-pack  ← write (push handshake)
///   POST /<owner>/<repo>.git/git-receive-pack                     ← write (push data)
///
/// codeload, raw, objects host paths look like
/// `/<owner>/<repo>/...` and are always read-only — `git clone` /
/// `git fetch` and tarball downloads. Allowed unconditionally; no
/// owner/repo check.
fn github_smart_decision(request: &[u8], allowed_repos: &[String]) -> GithubSmartDecision {
    let line_end = match request.windows(2).position(|w| w == b"\r\n") {
        Some(p) => p,
        None => return GithubSmartDecision::Malformed,
    };
    let line = match std::str::from_utf8(&request[..line_end]) {
        Ok(s) => s,
        Err(_) => return GithubSmartDecision::Malformed,
    };
    // METHOD path HTTP/1.1
    let mut parts = line.split_ascii_whitespace();
    let method = match parts.next() {
        Some(m) => m,
        None => return GithubSmartDecision::Malformed,
    };
    let path = match parts.next() {
        Some(p) => p,
        None => return GithubSmartDecision::Malformed,
    };
    let (path_no_query, query) = match path.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path, ""),
    };
    let trimmed = path_no_query.trim_start_matches('/');

    // Reject `..` traversal up front. Server normalisation would
    // otherwise pick a different repo than the one we checked.
    for seg in trimmed.split('/') {
        if seg == ".." {
            return GithubSmartDecision::Deny(format!(
                "agent-vm: path {path:?} contains '..' (traversal rejected)"
            ));
        }
    }

    // Identify the *operation*. Only the write paths need an allow-
    // list check; everything else (reads, browsing, archive downloads)
    // passes through.
    let is_push_data = method == "POST" && path_no_query.ends_with("/git-receive-pack");
    let is_push_handshake = method == "GET"
        && path_no_query.ends_with("/info/refs")
        && query.split('&').any(|kv| kv == "service=git-receive-pack");
    if !(is_push_data || is_push_handshake) {
        // Read / clone / fetch / browse / archive — allowed against
        // any repo.
        return GithubSmartDecision::Passthrough;
    }

    // It's a push. Extract owner/repo from the first two path
    // segments and check the allow-list.
    let mut it = trimmed.split('/');
    let owner = it.next().unwrap_or("");
    let repo_raw = it.next().unwrap_or("");
    if owner.is_empty() || repo_raw.is_empty() {
        return GithubSmartDecision::Deny(format!(
            "agent-vm: push path {path:?} doesn't name an owner/repo"
        ));
    }
    // Strip a single trailing `.git`; git smart paths are
    // `<repo>.git/...`. `strip_suffix` removes exactly one.
    let repo = repo_raw.strip_suffix(".git").unwrap_or(repo_raw);
    let slug = format!("{owner}/{repo}");
    if allowed_repos.iter().any(|a| a.eq_ignore_ascii_case(&slug)) {
        GithubSmartDecision::Passthrough
    } else {
        GithubSmartDecision::Deny(format!(
            "agent-vm: push to {slug:?} blocked by per-launch repo allow-list (reads are unrestricted; pushes are scoped). Allowed: {}",
            if allowed_repos.is_empty() {
                "(none)".into()
            } else {
                allowed_repos.join(", ")
            }
        ))
    }
}

fn read_gh_token(state_dir: &Path) -> Result<String> {
    let p = secrets::gh_token_path(state_dir);
    let s = std::fs::read_to_string(&p)
        .with_context(|| format!("reading {}", p.display()))?;
    Ok(s.trim().to_string())
}

/// Substitute `GH_TOKEN_PLACEHOLDER` in an Authorization header value
/// with `real_token`, handling both:
/// - `token <PLACEHOLDER>` / `Bearer <PLACEHOLDER>` — literal
///   substring, simple `replace`.
/// - `Basic base64(x-access-token:<PLACEHOLDER>)` — git's HTTP basic
///   auth scheme. The placeholder is base64-encoded inside the value,
///   so a literal replace would miss it; decode, substitute, re-encode.
///
/// Falls back to the literal-replace result for any value that isn't
/// recognisable as Basic auth, so non-GitHub callers' headers are
/// touched as little as possible.
fn substitute_authorization_header(value: &str, real_token: &str) -> String {
    if let Some(b64) = value.strip_prefix("Basic ").or_else(|| value.strip_prefix("basic ")) {
        use base64::Engine as _;
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
            if let Ok(s) = std::str::from_utf8(&decoded) {
                if s.contains(secrets::GH_TOKEN_PLACEHOLDER) {
                    let sub = s.replace(secrets::GH_TOKEN_PLACEHOLDER, real_token);
                    let re = base64::engine::general_purpose::STANDARD.encode(sub.as_bytes());
                    return format!("Basic {re}");
                }
            }
        }
    }
    value.replace(secrets::GH_TOKEN_PLACEHOLDER, real_token)
}

fn write_response(bytes: &[u8]) -> Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(bytes).context("writing response to stdout")?;
    out.flush().ok();
    Ok(())
}

fn refresh_anthropic(state_dir: &Path) -> Result<Vec<u8>> {
    trigger_host_refresh("claude", &["-p", "hi", "--model", "sonnet"])?;

    let host_path = host_claude_creds_path().context("HOME not set")?;
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

    let token_file = secrets::anthropic_token_path(state_dir);
    if let Some(parent) = token_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&token_file, new_access.as_bytes(), 0o600)?;

    // The in-VM Claude writes the refresh response into its local
    // credentials.json. Returning placeholders in both token fields
    // means the next API request gets routed through the substitution
    // path again, where the proxy swaps for the freshly-rotated bearer
    // it just read from the token file above.
    let body = json!({
        "access_token": secrets::ANTHROPIC_ACCESS_PLACEHOLDER,
        "refresh_token": secrets::ANTHROPIC_REFRESH_PLACEHOLDER,
        "expires_in": derive_expires_in(&expires_at),
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

    let host_path = host_codex_auth_path().context("HOME not set")?;
    let raw = std::fs::read_to_string(&host_path)
        .with_context(|| format!("reading {}", host_path.display()))?;
    let json: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", host_path.display()))?;

    let new_access = json
        .pointer("/tokens/access_token")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("OPENAI_API_KEY").and_then(|v| v.as_str()))
        .context("rotated host codex auth missing tokens.access_token or OPENAI_API_KEY")?;

    let token_file = secrets::openai_token_path(state_dir);
    if let Some(parent) = token_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&token_file, new_access.as_bytes(), 0o600)?;

    let body = json!({
        "access_token": secrets::OPENAI_ACCESS_PLACEHOLDER,
        "refresh_token": secrets::OPENAI_REFRESH_PLACEHOLDER,
        "id_token": secrets::OPENAI_ID_PLACEHOLDER,
        "expires_in": 3600,
        "token_type": "Bearer",
    });
    Ok(http_200_json(&serde_json::to_vec(&body)?))
}

fn trigger_host_refresh(cmd: &str, args: &[&str]) -> Result<()> {
    // Bounded wait so a hung host CLI doesn't keep the in-VM agent's
    // OAuth refresh waiting indefinitely (review #8). 90 s is enough
    // for normal claude/codex round-trips and small enough to surface
    // a problem before the guest agent's own timeout fires.
    use std::time::{Duration, Instant};
    const TIMEOUT: Duration = Duration::from_secs(90);

    let mut child = Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning host {cmd}"))?;
    let start = Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) => {
                let mut stderr = Vec::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read as _;
                    let _ = e.read_to_end(&mut stderr);
                }
                if !status.success() {
                    anyhow::bail!(
                        "host {cmd} failed (status {status}): {}",
                        String::from_utf8_lossy(&stderr)
                    );
                }
                return Ok(());
            }
            None => {
                if start.elapsed() >= TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!(
                        "host {cmd} did not return within {} s; killed",
                        TIMEOUT.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn looks_like_oauth_refresh(req: &[u8]) -> bool {
    std::str::from_utf8(req)
        .map(|s| s.lines().next().unwrap_or("").starts_with("POST "))
        .unwrap_or(false)
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
    let diff = (expires_at_ms - now_ms) / 1000;
    if diff <= 0 { 3600 } else { diff }
}

fn http_200_json(body: &[u8]) -> Vec<u8> {
    build_response(200, "OK", body)
}

fn error_response(code: u16, msg: &str) -> Vec<u8> {
    let body = format!("{{\"error\":{}}}", json!(msg));
    build_response(code, "Server Error", body.as_bytes())
}

fn build_response(code: u16, reason: &str, body: &[u8]) -> Vec<u8> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    let mut out = Vec::with_capacity(head.len() + body.len());
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(body);
    out
}
