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
            // Allow-listed: passthrough verbatim. Empty stdout tells
            // the proxy to forward the buffered prefix unchanged;
            // the secret layer swaps the placeholder for the real
            // bearer.
            GithubSmartOutcome::Authenticated => Vec::new(),
            // Not allow-listed: passthrough with Authorization
            // stripped. Non-empty, non-`HTTP/` stdout tells the
            // proxy "forward THESE bytes instead." GitHub treats
            // the request as third-party.
            GithubSmartOutcome::Anonymous => strip_authorization_from_request(&request),
            GithubSmartOutcome::Deny(msg) => error_response(403, &msg),
            GithubSmartOutcome::Malformed => {
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

    let access = github_access(&method, &path, allowed_repos);
    if let GithubAccess::Deny(reason) = &access {
        return Ok(error_response(403, reason));
    }
    let forward_with_auth = matches!(access, GithubAccess::Authenticated);

    // Only need to read the real token if we're going to forward with
    // auth. Anonymous requests don't need it.
    let real_token = if forward_with_auth {
        read_gh_token(state_dir).context("reading <state>.secrets/gh")?
    } else {
        String::new()
    };

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
            if forward_with_auth {
                // Substitute the placeholder → real token. Two forms:
                //   - `token <PLACEHOLDER>` / `Bearer <PLACEHOLDER>` —
                //     literal substring, handled by `replace`.
                //   - `Basic base64(x-access-token:<PLACEHOLDER>)` —
                //     the placeholder is base64-encoded, so a literal
                //     replace finds nothing. Decode, substitute, re-
                //     encode.
                let v = substitute_authorization_header(value, &real_token);
                req = req.header("Authorization", v);
            }
            // Anonymous: do NOT forward an Authorization header.
            // The guest sent the placeholder; we drop it. GitHub
            // then sees a third-party request.
            continue;
        }
        req = req.header(name, value);
    }
    if forward_with_auth && !had_authorization {
        // Guest sent no Authorization at all but the path is
        // allow-listed. Inject a Bearer with the real token — the
        // alternative is sending a silently-anonymous request that
        // gets 401, masking the agent's intent.
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

/// Result of a GitHub access-policy decision.
///
/// - `Authenticated` — forward with the user's real token (the proxy
///   substitutes `GH_TOKEN_PLACEHOLDER` for the host bearer on the
///   wire).
/// - `Anonymous` — forward WITHOUT the Authorization header. GitHub
///   then sees a third-party / unauthenticated request and serves
///   exactly what an external visitor would: public state succeeds,
///   private state returns 404 / 401, writes get 401.
/// - `Deny(reason)` — synthesize a 403 with `reason` (used for `..`
///   path traversal; otherwise the policy never denies outright, it
///   defers to GitHub's own auth enforcement).
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
enum GithubAccess {
    Authenticated,
    Anonymous,
    Deny(String),
}

/// Policy decision for an api.github.com request.
///
/// **Spec:** "allow-listed repos get my access; everything else gets
/// the same access a third-party / anonymous account would have."
///
/// Strategy: instead of trying to enumerate which paths are
/// public-vs-private (which would lag GitHub's API and break on every
/// surface change), we delegate to GitHub itself by **stripping the
/// Authorization header** for any request not naming an allow-listed
/// repo. GitHub then enforces public-vs-private as it does for
/// unauthenticated traffic.
///
/// Path-shape decisions:
/// - `/repos/<owner>/<repo>/...`: Authenticated iff `<owner>/<repo>`
///   is in the allow-list; otherwise Anonymous.
/// - `/user`, `/user/orgs`, `/user/orgs/...`: Authenticated. The
///   basic identity probe is what `gh auth status` needs; org list
///   is what `gh repo view org/x` uses.
/// - `/user/repos`, `/user/keys`, `/user/emails`, `/user/gpg_keys`,
///   any other `/user/*`: Anonymous (will 401 — matches "third
///   party can't see this").
/// - `/graphql`, `/search/*`, `/rate_limit`, `/meta`, `/markdown`:
///   Authenticated (utility endpoints; the GraphQL gap is the same
///   v1 limitation as before — bodies aren't filterable).
/// - `/users/<x>`, `/orgs/<x>`, `/notifications`, anything else:
///   Anonymous (third-party-visible info; private state hidden by
///   GitHub).
/// - `..` traversal anywhere: Deny.
fn github_access(method: &str, path: &str, allowed: &[String]) -> GithubAccess {
    let p = path.split_once('?').map(|(p, _)| p).unwrap_or(path);

    // Reject `..` traversal anywhere. GitHub server-normalises `..`,
    // so a crafted `/repos/<allowed>/.../../<victim>/private` could
    // otherwise resolve upstream to a different repo than we
    // approved. Cheap to reject up front for any method.
    for seg in p.split('/') {
        if seg == ".." {
            return GithubAccess::Deny(format!(
                "agent-vm: path {path:?} contains '..' (traversal rejected)"
            ));
        }
    }

    // Repo-scoped: allow-list determines auth.
    if let Some(rest) = p.strip_prefix("/repos/") {
        let mut it = rest.split('/');
        let owner = it.next().unwrap_or("");
        let repo = it.next().unwrap_or("");
        if owner.is_empty() || repo.is_empty() {
            // Malformed /repos/ path — go anonymous; GitHub returns 404.
            return GithubAccess::Anonymous;
        }
        let slug = format!("{owner}/{repo}");
        if allowed.iter().any(|a| a.eq_ignore_ascii_case(&slug)) {
            return GithubAccess::Authenticated;
        }
        // Method doesn't matter — third-party reads work for public
        // repos via Anonymous; writes 401, which is correct.
        return GithubAccess::Anonymous;
    }

    // Identity / org-membership probe: keep auth so gh CLI works.
    if p == "/user" || p == "/user/orgs" || p.starts_with("/user/orgs/") {
        return GithubAccess::Authenticated;
    }

    // All other /user/* paths leak host-user state to the agent if
    // we forward auth. Strip → GitHub returns 401, which matches the
    // user's spec ("third party wouldn't have access").
    if p.starts_with("/user/") {
        // Reads: GET /user/repos (private repo inventory), /user/keys,
        // /user/emails, /user/gpg_keys, etc. Writes: POST /user/keys,
        // DELETE /user/keys/N, etc. All go anonymous → 401.
        let _ = method;
        return GithubAccess::Anonymous;
    }

    // Utility endpoints — small, low-risk, gh-tooling-friendly.
    if matches!(
        p,
        "/graphql" | "/rate_limit" | "/meta" | "/markdown" | "/notifications"
    ) || p.starts_with("/search")
    {
        return GithubAccess::Authenticated;
    }

    // /users/<x>, /orgs/<x>, /repositories (id-based listing),
    // /licenses, /gitignore/templates, /emojis, /feeds, /events, …
    // — all third-party-visible read surfaces. Anonymous is fine.
    GithubAccess::Anonymous
}

/// Outcome of the smart-HTTP filter pass:
/// - `Authenticated`: passthrough verbatim (empty hook stdout — the
///   network secret-substitution layer swaps the placeholder for the
///   real bearer on the wire).
/// - `Anonymous`: passthrough with the buffered request's
///   Authorization header stripped (the new "modified passthrough"
///   verdict). GitHub then serves what an unauthenticated visitor
///   would see — public refs / blobs, 401 on private repos and
///   pushes.
/// - `Deny(reason)`: synthesized 403 (only on `..` traversal).
/// - `Malformed`: synthesized 400.
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
enum GithubSmartOutcome {
    Authenticated,
    Anonymous,
    Deny(String),
    Malformed,
}

/// Decide what to do with a git-smart-HTTP request to github.com /
/// codeload / raw / objects.
///
/// **Spec:** allow-listed repo → my access (Authenticated); any other
/// repo → third-party access (Anonymous). GitHub itself then enforces
/// public-vs-private: clone of a public repo works, clone of a
/// private non-allow-listed repo gets 401, push to any non-allow-
/// listed repo gets 401.
///
/// URL shapes that we look at:
///   GET  /<owner>/<repo>.git/info/refs?service=git-{upload,receive}-pack
///   POST /<owner>/<repo>.git/git-{upload,receive}-pack
///   GET  /<owner>/<repo>/...                      (codeload / raw / objects)
fn github_smart_decision(request: &[u8], allowed_repos: &[String]) -> GithubSmartOutcome {
    let line_end = match request.windows(2).position(|w| w == b"\r\n") {
        Some(p) => p,
        None => return GithubSmartOutcome::Malformed,
    };
    let line = match std::str::from_utf8(&request[..line_end]) {
        Ok(s) => s,
        Err(_) => return GithubSmartOutcome::Malformed,
    };
    let mut parts = line.split_ascii_whitespace();
    let _method = match parts.next() {
        Some(m) => m,
        None => return GithubSmartOutcome::Malformed,
    };
    let path = match parts.next() {
        Some(p) => p,
        None => return GithubSmartOutcome::Malformed,
    };
    let path_no_query = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
    let trimmed = path_no_query.trim_start_matches('/');

    for seg in trimmed.split('/') {
        if seg == ".." {
            return GithubSmartOutcome::Deny(format!(
                "agent-vm: path {path:?} contains '..' (traversal rejected)"
            ));
        }
    }

    // Extract owner/repo from the first two path segments. Strip a
    // single trailing `.git` (git smart paths are `<repo>.git/...`).
    let mut it = trimmed.split('/');
    let owner = it.next().unwrap_or("");
    let repo_raw = it.next().unwrap_or("");
    if owner.is_empty() || repo_raw.is_empty() {
        // Can't tell which repo — go anonymous, GitHub serves whatever
        // is public at that URL (typically 404 for malformed paths).
        return GithubSmartOutcome::Anonymous;
    }
    let repo = repo_raw.strip_suffix(".git").unwrap_or(repo_raw);
    let slug = format!("{owner}/{repo}");
    if allowed_repos.iter().any(|a| a.eq_ignore_ascii_case(&slug)) {
        GithubSmartOutcome::Authenticated
    } else {
        GithubSmartOutcome::Anonymous
    }
}

/// Return `request` with the `Authorization` header line removed.
/// Used to convert a buffered authenticated request into an
/// "anonymous" request that we can hand back to the proxy via the
/// passthrough-with-modified-bytes verdict.
///
/// Operates byte-precise on the header block (terminator
/// `\r\n\r\n`), preserves the request body verbatim, doesn't try to
/// re-parse anything else. Case-insensitive on the header name.
fn strip_authorization_from_request(request: &[u8]) -> Vec<u8> {
    let hdr_end = match request.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(p) => p,
        None => return request.to_vec(), // malformed; pass through
    };
    let (head, rest) = request.split_at(hdr_end);
    // rest starts with "\r\n\r\n"; keep that + body verbatim.

    let mut out: Vec<u8> = Vec::with_capacity(request.len());
    let mut cursor = 0usize;
    // Iterate header lines, skipping any whose name is "authorization".
    // Note: the LAST header in `head` typically has no trailing \r\n
    // (its CRLF is in `rest` as part of the \r\n\r\n separator) — so
    // we handle that case explicitly, checking auth before deciding
    // whether to emit it.
    while cursor < head.len() {
        let (line, next_cursor, has_trailing_crlf) =
            match head[cursor..].windows(2).position(|w| w == b"\r\n") {
                Some(p) => (&head[cursor..cursor + p], cursor + p + 2, true),
                None => (&head[cursor..], head.len(), false),
            };
        let is_auth = line
            .iter()
            .position(|&b| b == b':')
            .map(|colon| line[..colon].eq_ignore_ascii_case(b"authorization"))
            .unwrap_or(false);
        if !is_auth {
            out.extend_from_slice(line);
            if has_trailing_crlf {
                out.extend_from_slice(b"\r\n");
            }
        }
        cursor = next_cursor;
    }
    // Append the body separator + body unchanged.
    out.extend_from_slice(rest);
    out
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

// ─── tests ────────────────────────────────────────────────────────────
//
// Focus: the per-launch GitHub allow-list policy. This is the security
// surface — getting it wrong silently lets an in-VM agent push to or
// mutate repos the user didn't list. Cover the matrix:
//
//   axis            | values
//   ----------------|-----------------------------------------------
//   method          | GET, HEAD, POST, PATCH, PUT, DELETE
//   path category   | /repos/<o>/<r>/..., /graphql, /user, /user/repos,
//                   |   /user/keys, /markdown, /search, /admin, /...
//   allow-list      | empty, contains slug, contains other slug
//   traversal       | clean, .. anywhere
//   case            | uppercase/lowercase owner/repo
//
// For git smart-HTTP: discriminate clone/fetch (allow) from push
// (allow-list). Method+path+query distinguish them.

#[cfg(test)]
mod tests {
    use super::*;

    fn al(slugs: &[&str]) -> Vec<String> {
        slugs.iter().map(|s| s.to_string()).collect()
    }

    // ── github_access: allow-listed = my access ───────────────────

    #[test]
    fn gh_access_allow_listed_repo_is_authenticated() {
        let allowed = al(&["wirenboard/agent-vm"]);
        for m in ["GET", "HEAD", "POST", "PATCH", "PUT", "DELETE"] {
            assert_eq!(
                github_access(m, "/repos/wirenboard/agent-vm", &allowed),
                GithubAccess::Authenticated,
                "{m} /repos/wirenboard/agent-vm should be Authenticated"
            );
            assert_eq!(
                github_access(m, "/repos/wirenboard/agent-vm/issues", &allowed),
                GithubAccess::Authenticated,
            );
        }
    }

    #[test]
    fn gh_access_other_repo_is_anonymous_any_method() {
        // The whole point: a non-allow-listed repo gets third-party
        // access. GitHub itself enforces public/private — public
        // reads succeed, private 404s, writes 401.
        let allowed = al(&["wirenboard/agent-vm"]);
        for m in ["GET", "HEAD", "POST", "PATCH", "PUT", "DELETE"] {
            assert_eq!(
                github_access(m, "/repos/octocat/Hello-World", &allowed),
                GithubAccess::Anonymous,
                "{m} on non-allow-listed repo should be Anonymous"
            );
            assert_eq!(
                github_access(m, "/repos/private/something/issues", &allowed),
                GithubAccess::Anonymous,
            );
        }
    }

    #[test]
    fn gh_access_allow_list_match_is_case_insensitive() {
        let allowed = al(&["WirenBoard/Agent-VM"]);
        assert_eq!(
            github_access("POST", "/repos/wirenboard/agent-vm/issues", &allowed),
            GithubAccess::Authenticated,
        );
        assert_eq!(
            github_access("DELETE", "/repos/WIRENBOARD/AGENT-VM", &allowed),
            GithubAccess::Authenticated,
        );
    }

    #[test]
    fn gh_access_user_identity_endpoints_authenticated() {
        // gh auth status / gh repo view org/x need these.
        let allowed = al(&[]);
        assert_eq!(github_access("GET", "/user", &allowed), GithubAccess::Authenticated);
        assert_eq!(
            github_access("GET", "/user/orgs", &allowed),
            GithubAccess::Authenticated
        );
        assert_eq!(
            github_access("GET", "/user/orgs/123", &allowed),
            GithubAccess::Authenticated
        );
    }

    #[test]
    fn gh_access_user_pii_endpoints_are_anonymous() {
        // Per spec: third party can't see /user/repos (would reveal
        // private repo inventory), /user/keys (SSH keys),
        // /user/emails (verified emails), /user/gpg_keys. Stripping
        // auth → GitHub 401, matching what a third party would get.
        let allowed = al(&[]);
        for path in [
            "/user/repos",
            "/user/keys",
            "/user/keys/123",
            "/user/emails",
            "/user/gpg_keys",
            "/user/something-future-we-dont-recognise",
        ] {
            assert_eq!(
                github_access("GET", path, &allowed),
                GithubAccess::Anonymous,
                "{path} should strip auth"
            );
            assert_eq!(github_access("POST", path, &allowed), GithubAccess::Anonymous);
        }
    }

    #[test]
    fn gh_access_utility_endpoints_authenticated() {
        let allowed = al(&[]);
        for path in [
            "/graphql",
            "/rate_limit",
            "/meta",
            "/markdown",
            "/notifications",
            "/search/issues?q=foo",
            "/search/repositories",
        ] {
            assert!(
                matches!(github_access("POST", path, &allowed), GithubAccess::Authenticated)
                    || matches!(github_access("GET", path, &allowed), GithubAccess::Authenticated),
                "{path} should be Authenticated"
            );
        }
    }

    #[test]
    fn gh_access_public_lookup_endpoints_are_anonymous() {
        // Reads of other users / orgs / etc. — third-party
        // access serves what's public, hides what's private.
        let allowed = al(&[]);
        for path in [
            "/users/octocat",
            "/users/octocat/repos",
            "/orgs/github",
            "/orgs/private-org/members",
            "/licenses",
            "/emojis",
        ] {
            assert_eq!(
                github_access("GET", path, &allowed),
                GithubAccess::Anonymous,
                "{path} should be Anonymous (third-party access)"
            );
        }
    }

    #[test]
    fn gh_access_traversal_is_denied() {
        let allowed = al(&["allowed/repo"]);
        assert!(matches!(
            github_access("GET", "/repos/allowed/repo/../../victim/private", &allowed),
            GithubAccess::Deny(_)
        ));
        assert!(matches!(
            github_access("POST", "/repos/allowed/repo/../../victim/issues", &allowed),
            GithubAccess::Deny(_)
        ));
        assert!(matches!(
            github_access("GET", "/../etc/passwd", &allowed),
            GithubAccess::Deny(_)
        ));
    }

    #[test]
    fn gh_access_query_string_stripped_for_path_match() {
        let allowed = al(&["wirenboard/agent-vm"]);
        assert_eq!(
            github_access("GET", "/repos/wirenboard/agent-vm?ref=main", &allowed),
            GithubAccess::Authenticated,
        );
        assert_eq!(
            github_access("POST", "/repos/octocat/Hello-World/issues?x=y", &allowed),
            GithubAccess::Anonymous,
        );
    }

    #[test]
    fn gh_access_malformed_repos_path_goes_anonymous() {
        // The old policy denied these outright; the new policy
        // defers to GitHub by stripping auth, and GitHub returns
        // 404 for shapes it doesn't recognise. Safer + simpler.
        let allowed = al(&["wirenboard/agent-vm"]);
        for path in ["/repos/", "/repos/owner", "/repos/owner/", "/repos//repo"] {
            assert_eq!(
                github_access("POST", path, &allowed),
                GithubAccess::Anonymous,
                "{path} should be Anonymous (GitHub returns 404)"
            );
        }
    }

    // ── github_smart_decision: smart-HTTP ─────────────────────────

    fn req(line: &str) -> Vec<u8> {
        format!("{line}\r\nHost: github.com\r\n\r\n").into_bytes()
    }

    #[test]
    fn smart_allow_listed_repo_is_authenticated_for_clone_and_push() {
        let allowed = al(&["wirenboard/agent-vm"]);
        // Clone handshake.
        assert_eq!(
            github_smart_decision(
                &req("GET /wirenboard/agent-vm.git/info/refs?service=git-upload-pack HTTP/1.1"),
                &allowed,
            ),
            GithubSmartOutcome::Authenticated,
        );
        // Push handshake.
        assert_eq!(
            github_smart_decision(
                &req("GET /wirenboard/agent-vm.git/info/refs?service=git-receive-pack HTTP/1.1"),
                &allowed,
            ),
            GithubSmartOutcome::Authenticated,
        );
        // Push data.
        assert_eq!(
            github_smart_decision(
                &req("POST /wirenboard/agent-vm.git/git-receive-pack HTTP/1.1"),
                &allowed,
            ),
            GithubSmartOutcome::Authenticated,
        );
    }

    #[test]
    fn smart_other_repo_is_anonymous_for_any_operation() {
        // Third-party model: clone of a public repo works (GitHub
        // serves it), private 401s, push always 401s. We hand back
        // the same "Anonymous" verdict for every op and let GitHub
        // enforce.
        let allowed = al(&["wirenboard/agent-vm"]);
        for line in [
            "GET /octocat/Hello-World.git/info/refs?service=git-upload-pack HTTP/1.1",
            "POST /octocat/Hello-World.git/git-upload-pack HTTP/1.1",
            "GET /octocat/Hello-World.git/info/refs?service=git-receive-pack HTTP/1.1",
            "POST /octocat/Hello-World.git/git-receive-pack HTTP/1.1",
            "GET /octocat/Hello-World/zip/refs/heads/master HTTP/1.1",
            "GET /octocat/Hello-World/main/README.md HTTP/1.1",
        ] {
            assert_eq!(
                github_smart_decision(&req(line), &allowed),
                GithubSmartOutcome::Anonymous,
                "expected Anonymous for: {line}"
            );
        }
    }

    #[test]
    fn smart_dot_git_suffix_is_stripped_once_only() {
        let allowed = al(&["owner/repo.git"]);
        // Allow-list is literally `owner/repo.git` (silly but legal).
        // smart path is `/owner/repo.git.git/...`. After stripping
        // ONE `.git`, slug = `owner/repo.git`, matches the allow-list.
        assert_eq!(
            github_smart_decision(
                &req("POST /owner/repo.git.git/git-receive-pack HTTP/1.1"),
                &allowed,
            ),
            GithubSmartOutcome::Authenticated,
        );
    }

    #[test]
    fn smart_traversal_is_denied() {
        let allowed = al(&["allowed/repo"]);
        assert!(matches!(
            github_smart_decision(
                &req(
                    "POST /allowed/repo.git/../../victim/private.git/git-receive-pack HTTP/1.1"
                ),
                &allowed,
            ),
            GithubSmartOutcome::Deny(_),
        ));
    }

    #[test]
    fn smart_case_insensitive_allow_list() {
        let allowed = al(&["WirenBoard/Agent-VM"]);
        assert_eq!(
            github_smart_decision(
                &req("POST /wirenboard/agent-vm.git/git-receive-pack HTTP/1.1"),
                &allowed,
            ),
            GithubSmartOutcome::Authenticated,
        );
    }

    #[test]
    fn smart_malformed_request_is_flagged() {
        for r in [
            b"GET /foo HTTP/1.1".as_slice(),
            b"".as_slice(),
            b"GET\r\n".as_slice(),
        ] {
            assert!(matches!(
                github_smart_decision(r, &al(&["x/y"])),
                GithubSmartOutcome::Malformed,
            ));
        }
    }

    #[test]
    fn smart_malformed_owner_repo_path_is_anonymous() {
        // `/just-one-segment` doesn't name owner/repo. Old policy
        // denied; new policy goes Anonymous and lets GitHub 404.
        let allowed = al(&["x/y"]);
        assert_eq!(
            github_smart_decision(
                &req("GET /just-one-segment HTTP/1.1"),
                &allowed,
            ),
            GithubSmartOutcome::Anonymous,
        );
    }

    // ── strip_authorization_from_request ─────────────────────────

    #[test]
    fn strip_auth_removes_the_header_keeps_body() {
        let r = b"POST /repos/x/y/issues HTTP/1.1\r\n\
                  Host: api.github.com\r\n\
                  Authorization: token MSB_PLACEHOLDER_GH_TOKEN_v1\r\n\
                  Content-Type: application/json\r\n\
                  Content-Length: 11\r\n\
                  \r\n\
                  {\"title\":1}";
        let out = strip_authorization_from_request(r);
        let s = std::str::from_utf8(&out).unwrap();
        // Authorization line gone.
        assert!(!s.to_ascii_lowercase().contains("authorization:"));
        // Other headers preserved.
        assert!(s.contains("Host: api.github.com"));
        assert!(s.contains("Content-Type: application/json"));
        assert!(s.contains("Content-Length: 11"));
        // Body preserved verbatim.
        assert!(s.ends_with("\r\n\r\n{\"title\":1}"));
        // Placeholder absent at any layer.
        assert!(!s.contains("MSB_PLACEHOLDER_GH_TOKEN_v1"));
    }

    #[test]
    fn strip_auth_case_insensitive_on_header_name() {
        let r = b"GET /x HTTP/1.1\r\n\
                  authorization: Bearer X\r\n\
                  AUTHORIZATION: Bearer Y\r\n\
                  AuThOrIzAtIoN: Bearer Z\r\n\
                  Host: api.github.com\r\n\r\n";
        let out = strip_authorization_from_request(r);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(!s.to_ascii_lowercase().contains("authorization:"));
        assert!(s.contains("Host: api.github.com"));
    }

    #[test]
    fn strip_auth_no_auth_present_is_noop() {
        let r = b"GET /x HTTP/1.1\r\nHost: api.github.com\r\nUser-Agent: gh\r\n\r\n";
        let out = strip_authorization_from_request(r);
        assert_eq!(out, r);
    }

    #[test]
    fn strip_auth_malformed_no_separator_returns_input() {
        let r = b"GET /x HTTP/1.1\r\nAuthorization: Bearer X";
        let out = strip_authorization_from_request(r);
        // We don't try to parse beyond the separator; if it's
        // missing, pass through unchanged so the proxy at least
        // forwards SOMETHING.
        assert_eq!(out, r);
    }

    #[test]
    fn strip_auth_preserves_request_line_and_other_colons() {
        // Some header values contain `:` (e.g. Cookie name=URL). The
        // split-on-first-`:` for the header NAME must not be tricked.
        let r = b"POST /repos/x/y HTTP/1.1\r\n\
                  Cookie: a=b; url=http://example.com/path\r\n\
                  Authorization: token PLACEHOLDER\r\n\
                  \r\n";
        let out = strip_authorization_from_request(r);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.starts_with("POST /repos/x/y HTTP/1.1\r\n"));
        assert!(s.contains("Cookie: a=b; url=http://example.com/path"));
        assert!(!s.to_ascii_lowercase().contains("authorization:"));
    }

    // ── substitute_authorization_header ───────────────────────────

    #[test]
    fn auth_substitute_bearer_is_literal_replace() {
        let out = substitute_authorization_header(
            &format!("Bearer {}", secrets::GH_TOKEN_PLACEHOLDER),
            "real_token_xyz",
        );
        assert_eq!(out, "Bearer real_token_xyz");
    }

    #[test]
    fn auth_substitute_token_form_is_literal_replace() {
        let out = substitute_authorization_header(
            &format!("token {}", secrets::GH_TOKEN_PLACEHOLDER),
            "real_token_xyz",
        );
        assert_eq!(out, "token real_token_xyz");
    }

    #[test]
    fn auth_substitute_basic_decodes_encodes() {
        use base64::Engine as _;
        let basic_value = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode(format!("x-access-token:{}", secrets::GH_TOKEN_PLACEHOLDER).as_bytes())
        );
        let out = substitute_authorization_header(&basic_value, "real_xyz");
        // Round-trip: decode the result and check it contains the real token.
        let stripped = out.strip_prefix("Basic ").expect("Basic prefix preserved");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(stripped.as_bytes())
            .expect("output is valid base64");
        let s = std::str::from_utf8(&decoded).expect("utf8");
        assert_eq!(s, "x-access-token:real_xyz");
        // And the placeholder is NOT in the output at any layer.
        assert!(!out.contains(secrets::GH_TOKEN_PLACEHOLDER));
        assert!(!s.contains(secrets::GH_TOKEN_PLACEHOLDER));
    }

    // ── parse_http_request ────────────────────────────────────────

    #[test]
    fn parse_http_request_basic_get_no_body() {
        let req = b"GET /repos/o/r HTTP/1.1\r\nHost: api.github.com\r\nUser-Agent: gh/2\r\n\r\n";
        let (method, path, headers, body) = parse_http_request(req).unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/repos/o/r");
        assert!(body.is_empty());
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0], ("Host".into(), "api.github.com".into()));
        assert_eq!(headers[1], ("User-Agent".into(), "gh/2".into()));
    }

    #[test]
    fn parse_http_request_post_with_body() {
        let req = b"POST /graphql HTTP/1.1\r\nHost: api.github.com\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"query\":1}";
        let (method, path, headers, body) = parse_http_request(req).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/graphql");
        assert_eq!(body, b"{\"query\":1}");
        assert_eq!(headers.len(), 3);
    }

    #[test]
    fn parse_http_request_header_value_with_colons_preserved() {
        // Authorization values commonly contain `:` — verify the
        // header split keeps everything after the first `:`.
        let req = b"GET /x HTTP/1.1\r\nAuthorization: Basic dXNlcjpwYXNz:extra\r\n\r\n";
        let (_m, _p, headers, _b) = parse_http_request(req).unwrap();
        let auth = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("Authorization"));
        assert_eq!(
            auth.map(|(_, v)| v.as_str()),
            Some("Basic dXNlcjpwYXNz:extra")
        );
    }

    #[test]
    fn parse_http_request_errors_on_missing_separator() {
        // No \r\n\r\n anywhere — can't find header/body boundary.
        let req = b"GET /x HTTP/1.1\r\nHost: api.github.com\r\n";
        assert!(parse_http_request(req).is_err());
    }

    #[test]
    fn parse_http_request_errors_on_empty_request_line() {
        let req = b"\r\nHost: api.github.com\r\n\r\n";
        let err = parse_http_request(req);
        assert!(err.is_err(), "empty request line must error");
    }

    #[test]
    fn parse_http_request_handles_extra_whitespace_in_headers() {
        // Header values are trimmed of surrounding whitespace.
        let req = b"GET /x HTTP/1.1\r\nFoo:   bar  \r\n\r\n";
        let (_m, _p, headers, _b) = parse_http_request(req).unwrap();
        assert_eq!(headers[0], ("Foo".into(), "bar".into()));
    }

    #[test]
    fn auth_substitute_basic_no_placeholder_passes_through() {
        // A `Basic ...` value that doesn't carry our placeholder
        // should not be re-encoded; preserve verbatim so we don't
        // silently mangle the caller's credentials.
        use base64::Engine as _;
        let untouched_basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(b"alice:hunter2")
        );
        let out = substitute_authorization_header(&untouched_basic, "real_xyz");
        assert_eq!(out, untouched_basic);
    }
}
