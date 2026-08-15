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
            // Allow-listed: forward the request unchanged EXCEPT we
            // inject `Connection: close` so the upstream tears down
            // the TCP after responding. This prevents the
            // keep-alive bypass: msb's Interceptor goes to
            // State::Disabled after one dispatch, so any subsequent
            // HTTP/1.1 request on the same connection would be
            // forwarded with the secret-substituted Authorization
            // (real token) directly to upstream, even if it
            // targets a different (non-allow-listed) repo.
            GithubSmartOutcome::Authenticated => set_connection_close(&request),
            // Not allow-listed: passthrough with Authorization
            // stripped. Non-empty, non-`HTTP/` stdout tells the
            // proxy "forward THESE bytes instead." Also injects
            // `Connection: close` for the same reason (otherwise
            // the next request on the same connection — e.g. a
            // libcurl retry — bypasses the hook). GitHub treats
            // the request as third-party.
            GithubSmartOutcome::Anonymous => {
                set_connection_close(&strip_authorization_from_request(&request))
            }
            GithubSmartOutcome::Deny(msg) => error_response(403, &msg),
            GithubSmartOutcome::Malformed => {
                error_response(400, "agent-vm github smart-HTTP filter: malformed request")
            }
        };
        write_response(&response)?;
        return Ok(());
    }

    // Which exact path is the token endpoint for this provider. Rules
    // are registered with *prefix* semantics, so this is also what
    // separates a refresh from its neighbours under the same prefix
    // (`/v1/oauth/token/revoke`, Claude Code's logout).
    let token_path = match args.sni.as_str() {
        secrets::ANTHROPIC_OAUTH_HOST => secrets::ANTHROPIC_OAUTH_TOKEN_PATH,
        secrets::OPENAI_OAUTH_HOST => secrets::OPENAI_OAUTH_TOKEN_PATH,
        other => {
            write_response(&error_response(
                500,
                &format!("agent-vm hook has no logic for SNI {other}"),
            ))?;
            return Ok(());
        }
    };

    if !looks_like_oauth_refresh(&request, token_path) {
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

    let (refreshed, host_fix) = match args.sni.as_str() {
        secrets::ANTHROPIC_OAUTH_HOST => (refresh_anthropic(&args.state_dir), "claude"),
        secrets::OPENAI_OAUTH_HOST => (refresh_openai(&args.state_dir), "codex login"),
        // Unreachable: `token_path` above already rejected other SNIs.
        other => {
            write_response(&error_response(
                500,
                &format!("agent-vm hook has no logic for SNI {other}"),
            ))?;
            return Ok(());
        }
    };

    // Anything that went wrong reading, parsing or rewriting the host
    // credential still owes the guest an answer. Returning `Err` here
    // would exit non-zero, which makes microsandbox drop the TLS
    // connection: the guest sees an opaque socket error and the reason
    // is logged host-side at debug level, where nobody looks. The
    // refusal response carries a reason the person in the terminal can
    // act on instead.
    //
    // The detail goes to stderr only, never into the response body — an
    // error chain here can carry a host CLI's own stderr, and the guest
    // is the one thing that must not see host output.
    let response = match refreshed {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), sni = %args.sni, "OAuth refresh hook failed");
            eprintln!("agent-vm: OAuth refresh for {} failed: {e:#}", args.sni);
            host_credentials_unusable_response(
                "the host credential could not be read or rotated",
                host_fix,
            )
        }
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
    let (method, raw_path, headers, body) = parse_http_request(request)
        .context("parsing intercepted github request")?;

    // RFC 7230 absolute-form (`GET https://api.github.com/repos/x`) is
    // legal and GitHub accepts it. msb normalises it before matching
    // rules, but the hook re-derives the upstream URL from this path,
    // so normalise here too — otherwise we'd concatenate it onto the
    // host and 502 on a malformed URL instead of applying policy.
    let path = normalize_origin_form(&raw_path).to_string();

    // `/graphql` carries its repo references in the body, not the
    // path — gh CLI does most reads (repo list/view, pr, issue) over
    // GraphQL, so it gets its own body-level allow-list filter. Only
    // POST is real GraphQL traffic; anything else goes anonymous.
    let (path_no_query, query_string) = match path.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path.as_str(), ""),
    };
    let access = if path_no_query == "/graphql" {
        // Our verdict comes from the body, so the body must be the only
        // thing GitHub can execute. Two guards on that:
        //
        //  * A query string is refused. We do not know that GitHub's
        //    GraphQL endpoint ignores `?query=`, and if it ever reads it
        //    (the graphql-ruby / Rails `params[:query]` idiom) the whole
        //    filter is bypassed by a benign-looking body. gh never sends
        //    one, so refusing costs nothing.
        //  * A non-JSON content type is refused, so a body encoding
        //    GitHub accepts but `serde_json` doesn't can't be judged on
        //    a parse failure of a different grammar.
        let content_type_is_json = headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("content-type")
                && v.to_ascii_lowercase().starts_with("application/json")
        });
        if method.eq_ignore_ascii_case("POST") && query_string.is_empty() && content_type_is_json {
            match crate::github_graphql::graphql_access(&body, allowed_repos) {
                crate::github_graphql::GraphqlAccess::Authenticated => {
                    GithubAccess::Authenticated
                }
                // Deny, not Anonymous. GitHub's GraphQL endpoint has no
                // anonymous tier: a stripped-Authorization query comes
                // back `403 API rate limit exceeded for <host IP>`,
                // which tells the agent nothing true. Same posture — no
                // token leaves — with a failure someone can act on.
                crate::github_graphql::GraphqlAccess::Denied(why) => GithubAccess::Deny(why),
            }
        } else {
            GithubAccess::Deny(
                "agent-vm: /graphql requires POST, no query string, and a JSON content type"
                    .to_string(),
            )
        }
    } else {
        github_access(&method, &path, allowed_repos)
    };
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

/// Map an absolute-form request target to origin-form. Anything else
/// is returned unchanged.
fn normalize_origin_form(target: &str) -> &str {
    for scheme in ["http://", "https://"] {
        if target.len() >= scheme.len() && target[..scheme.len()].eq_ignore_ascii_case(scheme) {
            let rest = &target[scheme.len()..];
            return match rest.find('/') {
                Some(slash) => &rest[slash..],
                None => "/",
            };
        }
    }
    target
}

#[cfg(test)]
mod origin_form_tests {
    use super::normalize_origin_form;

    #[test]
    fn absolute_form_is_reduced_to_the_path() {
        assert_eq!(
            normalize_origin_form("https://api.github.com/repos/a/b"),
            "/repos/a/b"
        );
        assert_eq!(normalize_origin_form("HTTPS://api.github.com/x?y=1"), "/x?y=1");
        assert_eq!(normalize_origin_form("http://api.github.com"), "/");
        assert_eq!(normalize_origin_form("/repos/a/b"), "/repos/a/b");
    }
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
/// - `/rate_limit`, `/meta`, `/markdown`: Authenticated (utility
///   endpoints, not user-scoped). `/graphql` is handled by the
///   body-level filter in `github_graphql` before this function is
///   consulted; here it falls through to Anonymous.
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

    // Search and notifications are user-scoped under auth and would
    // leak private repo inventory / personal state to the agent.
    // Strip auth so the agent gets exactly what a third party sees:
    // /search/* → public-only results; /notifications → 401.
    // Agents needing in-private-repo search can `git grep` inside
    // the bind-mounted project (works directly on disk).
    if p == "/notifications"
        || p.starts_with("/notifications/")
        || p == "/search"
        || p.starts_with("/search/")
    {
        return GithubAccess::Anonymous;
    }

    // Other utility endpoints are not user-scoped and are safe to
    // forward authenticated (gh tooling uses them). `/graphql` is NOT
    // here: it names repos in the request *body*, so it has its own
    // filter (`github_graphql::graphql_access`) — this path-only
    // policy answers Anonymous for it as defense in depth.
    if matches!(p, "/rate_limit" | "/meta" | "/markdown") {
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
/// Inject (or overwrite) a `Connection: close` header in the request.
///
/// **Why:** msb's Interceptor handles one request per connection.
/// After dispatch its state becomes Disabled and subsequent HTTP/1.1
/// requests on the same TCP/TLS connection bypass the hook entirely
/// — the proxy forwards the secret-substitution layer's output
/// (with the real token already in the Authorization header) straight
/// to upstream. Forcing `Connection: close` makes the server tear
/// down the connection after responding, so any follow-up request
/// opens a fresh TCP, creating a fresh Interceptor that re-evaluates
/// the policy. This is the dominant real-world bypass: libcurl
/// (git's HTTP backend) reuses connections aggressively, and gh /
/// git clone do multiple requests per connection.
///
/// Operates byte-precise on the header block (terminator `\r\n\r\n`),
/// preserves the request body verbatim, doesn't try to re-parse
/// anything else.
fn set_connection_close(request: &[u8]) -> Vec<u8> {
    let hdr_end = match request.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(p) => p,
        None => return request.to_vec(), // malformed; pass through
    };
    let (head, rest) = request.split_at(hdr_end);

    // Collect kept lines, skipping any existing Connection / Keep-Alive
    // / Proxy-Connection headers — we replace them with our own
    // single `Connection: close`.
    let mut kept: Vec<&[u8]> = Vec::new();
    let mut cursor = 0usize;
    while cursor < head.len() {
        let (line, next_cursor) = match head[cursor..].windows(2).position(|w| w == b"\r\n") {
            Some(p) => (&head[cursor..cursor + p], cursor + p + 2),
            None => (&head[cursor..], head.len()),
        };
        let should_drop = line
            .iter()
            .position(|&b| b == b':')
            .map(|colon| {
                let name = &line[..colon];
                name.eq_ignore_ascii_case(b"connection")
                    || name.eq_ignore_ascii_case(b"keep-alive")
                    || name.eq_ignore_ascii_case(b"proxy-connection")
            })
            .unwrap_or(false);
        if !should_drop {
            kept.push(line);
        }
        cursor = next_cursor;
    }

    let mut out: Vec<u8> = Vec::with_capacity(request.len() + 32);
    for (i, line) in kept.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(line);
    }
    // Always emit the Connection: close header (after the last kept
    // line, before the rest's \r\n\r\n). If `kept` is empty (no
    // request line — malformed), skip prepending the join \r\n.
    if !kept.is_empty() {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"Connection: close");
    out.extend_from_slice(rest);
    out
}

fn strip_authorization_from_request(request: &[u8]) -> Vec<u8> {
    let hdr_end = match request.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(p) => p,
        None => return request.to_vec(), // malformed; pass through
    };
    let (head, rest) = request.split_at(hdr_end);
    // rest starts with "\r\n\r\n"; keep that + body verbatim.

    // Collect lines (request line + headers) that we want to keep.
    // Note: the LAST line in `head` has no trailing \r\n in `head`
    // (that \r\n is part of the \r\n\r\n in `rest`). We collect lines
    // verbatim then join with \r\n at the end — that way dropping the
    // last line is naturally handled: the previous line we kept does
    // not get a trailing \r\n, and `rest` supplies the \r\n that
    // terminates the last kept header.
    let mut kept: Vec<&[u8]> = Vec::new();
    let mut cursor = 0usize;
    while cursor < head.len() {
        let (line, next_cursor) = match head[cursor..].windows(2).position(|w| w == b"\r\n") {
            Some(p) => (&head[cursor..cursor + p], cursor + p + 2),
            None => (&head[cursor..], head.len()),
        };
        let is_auth = line
            .iter()
            .position(|&b| b == b':')
            .map(|colon| line[..colon].eq_ignore_ascii_case(b"authorization"))
            .unwrap_or(false);
        if !is_auth {
            kept.push(line);
        }
        cursor = next_cursor;
    }

    let mut out: Vec<u8> = Vec::with_capacity(request.len());
    for (i, line) in kept.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(line);
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

/// Public entry point for an Anthropic OAuth-refresh interception.
///
/// Thin wrapper binding the one real side effect — spawning the host
/// `claude` CLI, which rotates `~/.claude/.credentials.json` when its
/// own token is near expiry. The decisions live in
/// [`refresh_anthropic_inner`] and the response synthesis in
/// [`rotate_anthropic`]; keeping the side effect out of both is what
/// makes the rotation path deterministically testable without a live
/// `claude` session or a real expiring token (PLAN.md A1).
fn refresh_anthropic(state_dir: &Path) -> Result<Vec<u8>> {
    let host_path = host_claude_creds_path().context("HOME not set")?;
    refresh_anthropic_inner(state_dir, &host_path, &|| {
        trigger_host_refresh(
            "claude",
            &["-p", "hi", "--model", "sonnet"],
            ANTHROPIC_HOST_REFRESH_TIMEOUT,
        )
    })
}

/// [`refresh_anthropic`] with the one real side effect — spawning the
/// host `claude` CLI — behind a callback, so tests can pin the *order*
/// of the decisions (which is where the cost and the correctness both
/// live) without a live Claude session or a real expiring token.
fn refresh_anthropic_inner(
    state_dir: &Path,
    host_path: &Path,
    rotate_host: &dyn Fn() -> Result<()>,
) -> Result<Vec<u8>> {
    let read_host = || {
        std::fs::read_to_string(host_path)
            .with_context(|| format!("reading {}", host_path.display()))
    };
    // Re-wrap with the concrete path so a parse/extract failure in the pure fn
    // still names which exact host file was bad (the pure fn uses a fixed label).
    let rotate = |raw: &str| {
        rotate_anthropic(state_dir, raw)
            .with_context(|| format!("rotating Anthropic token from {}", host_path.display()))
    };

    // A guest refresh does *not* imply the host token needs rotating.
    // The guest asks whenever *its* copy is near expiry, which happens
    // routinely for reasons that have nothing to do with the host token
    // — a state dir seeded from an older credential file, or another
    // process having already rotated the host's. When the host token is
    // still good, the only thing to do is re-sync the per-project token
    // file and answer; spawning `claude -p` there costs a real inference
    // round trip and buys nothing (the host CLI wouldn't rotate either,
    // for exactly the same reason: it is not near expiry).
    //
    // Rewriting the token file here without holding the single-flight
    // lock is safe: the only other writer is the locked path below, and
    // both write the same thing — whatever the host file currently says
    // — via `atomic_write`. A racing reader gets one whole token or the
    // other, and this branch only runs when the host token has more than
    // [`HOST_TOKEN_ROTATION_MARGIN`] of life, i.e. it cannot be the
    // stale side of a rotation another process just finished.
    let raw = read_host()?;
    if !anthropic_host_token_needs_rotation(&raw) {
        return rotate(&raw);
    }

    // Single-flight: serialize host-side rotations for this provider so
    // two racing in-guest refreshes don't each spawn `claude -p`. The
    // first waiter rotates; the second, on acquiring the lock, re-reads
    // the host file, finds it already rotated and skips its own host
    // CLI. The lock is Anthropic-specific so a concurrent OpenAI refresh
    // isn't blocked.
    let _flight = RefreshLock::acquire(state_dir, secrets::REFRESH_LOCK_ANTHROPIC)?;
    let raw = read_host()?;
    if anthropic_host_token_needs_rotation(&raw)
        && !host_refresh_damped(state_dir, secrets::REFRESH_STAMP_ANTHROPIC)
    {
        // Stamp before the spawn, not after, so a crash mid-rotation
        // still damps the next request.
        note_host_refresh_attempt(state_dir, secrets::REFRESH_STAMP_ANTHROPIC);
        if let Err(e) = rotate_host() {
            // A failed host CLI is not automatically fatal. `claude -p`
            // refreshes the token *before* it makes its inference call,
            // so the common non-zero exits (overloaded, rate limited,
            // no credit) come back with the rotation already done — and
            // even when it truly did nothing, the host token may still
            // have hours on it, the guest having asked only because
            // *its* copy was stale. So: log, and fall through to the
            // same re-read-and-answer path as success. Erroring out
            // here would instead drop the connection under the guest.
            tracing::warn!(error = %format!("{e:#}"), "host Anthropic rotation failed");
            eprintln!("agent-vm: host Anthropic token rotation failed: {e:#}");
        }
        // Re-read either way — see above, the file may well have been
        // rotated by a run that then exited non-zero.
        return rotate(&read_host()?);
    }
    rotate(&raw)
}

/// Minimum spacing between host CLI spawns for one project.
///
/// The rotation decision alone is not a sufficient damper. A host CLI
/// that does not actually rotate — it exits non-zero, or
/// exits 0 while the credential lives somewhere we don't read — leaves
/// us answering with whatever the host token still has, and the guest
/// comes straight back. Without spacing that is one real inference call
/// per guest request.
///
/// Sized against the window it has to work in, which is short: `claude`
/// only refreshes inside its own 5-minute expiry window, and
/// [`anthropic_expires_in`] refuses anything at or under that, so the
/// entire period in which a host rotation is both attempted *and*
/// useful is the last 300 s of the token's life — the same period in
/// which every guest request is already failing. 30 s leaves ~10
/// recovery attempts in that window (a transient host failure gets
/// several more shots) while still turning a per-request storm into a
/// trickle. Deliberately its own number rather than a share of a host
/// CLI timeout: those answer a different question — how long a rotation
/// may legitimately take — and tying the two made this far too coarse
/// for the window it has to work in.
const HOST_REFRESH_MIN_SPACING: std::time::Duration = std::time::Duration::from_secs(30);

/// Record that a host-side rotation was just attempted for this project.
///
/// Deliberately a *separate* file from the token file: the token file is
/// rewritten on every refresh, including the ones that never spawn
/// anything, so its mtime says nothing about host CLI attempts.
/// Best-effort — a failure here only costs an extra spawn.
fn note_host_refresh_attempt(state_dir: &Path, stamp: &str) {
    let path = secrets::host_refresh_stamp_path(state_dir, stamp);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = atomic_write(&path, b"", 0o600);
}

/// True when a host CLI ran too recently for another spawn to be worth
/// it — see [`HOST_REFRESH_MIN_SPACING`].
///
/// Any error (no stamp yet, clock skew putting the mtime in the future)
/// answers `false`, i.e. allow the spawn: over-damping stalls a session
/// that a retry could have saved.
fn host_refresh_damped(state_dir: &Path, stamp: &str) -> bool {
    let path = secrets::host_refresh_stamp_path(state_dir, stamp);
    let Ok(stamped_at) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
        return false;
    };
    stamped_at
        .elapsed()
        .map(|age| age <= HOST_REFRESH_MIN_SPACING)
        .unwrap_or(false)
}

/// How close to expiry the *host* access token has to be before driving
/// a host-side rotation is worth a `claude -p` round trip.
///
/// Deliberately wider than Claude Code's own 5-minute proactive refresh
/// window. The in-guest agent starts asking at T−5 min; if we used the
/// same 5 minutes the two would sit on the boundary and trade a useless
/// round trip (we decline to rotate → we answer with the ~5 min the host
/// token has left → the guest immediately considers that expired and
/// asks again). Ten minutes puts our decision unambiguously on the
/// "rotate" side by the time the guest asks. The cost of the wider
/// window is a `claude -p` in the 5–10 min band that the host CLI may
/// itself decline to act on — rare, and self-correcting on the next ask.
const HOST_TOKEN_ROTATION_MARGIN: std::time::Duration = std::time::Duration::from_secs(600);

/// True when the host's `claudeAiOauth` access token is at (or near)
/// expiry and a host-side `claude -p` could actually rotate it.
///
/// Conservative on every uncertainty — unparseable file, missing or
/// zero `expiresAt` — because the caller's fallback is "spawn the host
/// CLI", which is merely expensive, whereas skipping a rotation that
/// *was* needed hands the guest a dead bearer.
fn anthropic_host_token_needs_rotation(host_creds_json: &str) -> bool {
    let Ok(json) = serde_json::from_str::<Value>(host_creds_json) else {
        return true;
    };
    let Some(expires_at_ms) = expires_at_ms(json.pointer("/claudeAiOauth/expiresAt")) else {
        return true;
    };
    expires_at_ms.saturating_sub(now_unix_ms()) <= HOST_TOKEN_ROTATION_MARGIN.as_millis() as i64
}

/// Read a `claudeAiOauth.expiresAt` (ms since the epoch) out of the host
/// credential file, or `None` when it is absent, zero, or not a number
/// we can use.
///
/// Both numeric JSON shapes are accepted: `serde_json` hands back an
/// integer for `1770000000000` but a float for `1.77e12`, and
/// `as_i64()` alone silently misses the latter — which used to fall
/// through to a fabricated one-hour expiry.
fn expires_at_ms(field: Option<&Value>) -> Option<i64> {
    let ms = match field {
        Some(v) if v.is_i64() || v.is_u64() => v.as_i64(),
        Some(v) => v.as_f64().map(|f| f as i64),
        None => None,
    }?;
    (ms > 0).then_some(ms)
}

/// Pure rotation step for Anthropic: parse the (already-rotated) host
/// `.credentials.json` text, rewrite the per-project token file with
/// the fresh real bearer, and synthesize the OAuth refresh response
/// that carries *placeholders* (never the real bearer) back to the
/// in-VM agent.
///
/// Split out from [`refresh_anthropic`] so tests can drive a simulated
/// rotation by passing the rotated-file contents directly, with no host
/// CLI spawn and no `$HOME` credential file. Runtime behavior is
/// identical: `refresh_anthropic` calls this with the bytes it just
/// read from the real host file.
fn rotate_anthropic(state_dir: &Path, host_creds_json: &str) -> Result<Vec<u8>> {
    let json: Value = serde_json::from_str(host_creds_json)
        .context("parsing rotated host .credentials.json")?;
    let oauth = json
        .get("claudeAiOauth")
        .context("rotated host .credentials.json missing claudeAiOauth")?;
    let new_access = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())
        .context("rotated host claudeAiOauth missing accessToken")?;

    // A host token with no usable life left after the rotation attempt
    // is not something to paper over. Refuse, and let the guest's own
    // error handling take it from there.
    let expires_in = match anthropic_expires_in(oauth.get("expiresAt")) {
        Ok(seconds) => seconds,
        Err(why) => return Ok(host_credentials_unusable_response(why, "claude")),
    };

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
        "expires_in": expires_in,
        "token_type": "Bearer",
        "scope": oauth_scope_string(oauth.get("scopes")),
    });
    Ok(http_200_json(&serde_json::to_vec(&body)?))
}

/// Seconds of *usable* life left on the host access token, or `None`
/// when the answer would do the guest more harm than a refusal.
///
/// The guest writes `expiresAt = now + expires_in` and asks again five
/// minutes before that. So the value has to be both truthful and above
/// that window, and there are two ways to get it wrong:
///
///  * **Fabricating one.** The old code answered a missing or
///    already-past expiry with a flat 3600. Since the guest now
///    persists what we say (it did not before — that was the bug this
///    all sits on), that buys 55 minutes of confidence in a bearer that
///    401s on first use.
///  * **Answering below the guest's window.** Anything under five
///    minutes is *born* expired: the guest asks again on its very next
///    request, and every ask is another hook invocation. That is the
///    condition to report, not to serve.
///
/// Both cases mean the same thing — the host rotation did not produce a
/// token worth handing over — so both return `None` and the caller
/// answers with [`host_credentials_unusable_response`].
fn anthropic_expires_in(expires_at_field: Option<&Value>) -> Result<i64, &'static str> {
    /// Claude Code's own proactive-refresh window: it treats a token as
    /// expired once `now + 5 min >= expiresAt`.
    const GUEST_REFRESH_WINDOW_SECS: i64 = 300;

    let Some(expires_at) = expires_at_ms(expires_at_field) else {
        return Err("the host credential file carries no usable expiresAt");
    };
    let remaining = expires_at.saturating_sub(now_unix_ms()) / 1000;
    if remaining <= GUEST_REFRESH_WINDOW_SECS {
        return Err("the host credential has expired or is about to");
    }
    Ok(remaining)
}

/// Answer for "the host credential is not usable and we could not fix
/// it" — the guest asked us to refresh, the host-side rotation ran (or
/// was not needed), and what came back still has no life in it.
///
/// A synthesized HTTP response, not a hook error: an error exit makes
/// microsandbox drop the TLS connection, which reaches the guest as an
/// opaque socket failure and logs only at debug level on the host.
///
/// Deliberately **not** `invalid_grant`. Claude Code treats that code as
/// proof the refresh token itself is dead and blanks `accessToken` /
/// `refreshToken` / `expiresAt` in its credentials file — a destructive
/// reaction to what is usually a transient host-side condition. `503` +
/// `temporarily_unavailable` fails the refresh without that side effect,
/// and the description names the fix.
fn host_credentials_unusable_response(why: &str, host_fix: &str) -> Vec<u8> {
    let text = format!(
        "agent-vm: {why}, and a host-side rotation did not fix it; \
         run `{host_fix}` on the host to re-authenticate"
    );
    let body = json!({
        "error": "temporarily_unavailable",
        "error_description": text,
        // go-gh and friends only render `message`; keep it in step with
        // `error_description` so no reader loses the reason.
        "message": text,
    });
    build_response(
        503,
        "Service Unavailable",
        &serde_json::to_vec(&body).unwrap_or_default(),
    )
}

/// Scopes Claude Code must see when the host credential file carries
/// none. `user:inference` is the load-bearing one (see
/// [`oauth_scope_string`]); `user:profile` comes with it on every real
/// claude.ai login.
const ANTHROPIC_FALLBACK_SCOPES: &str = "user:inference user:profile";

/// Render the host credential file's `scopes` as an OAuth `scope`
/// **string** — space-delimited, per RFC 6749 §3.3.
///
/// This is not cosmetic. The host file stores `scopes` as a JSON array,
/// and echoing that array straight back is what made every intercepted
/// rotation a silent no-op: Claude Code parses the field with
/// `typeof scope !== "string" ? [] : scope.split(" ")`, so an array
/// collapses to *no scopes*. It then gates persistence on the parsed
/// scopes containing `user:inference` — without it the refreshed token
/// and, critically, its new `expiresAt` are never written to
/// `.credentials.json`. The guest stays pinned at the old expiry, treats
/// itself as permanently expired, and re-refreshes ahead of every single
/// API request, each one round-tripping through this hook.
///
/// A string is passed through as-is (defensive: a future host file
/// storing the already-joined form stays correct), and an absent or
/// unusable value falls back to [`ANTHROPIC_FALLBACK_SCOPES`] rather
/// than an empty string, which would reproduce the same bug.
fn oauth_scope_string(scopes: Option<&Value>) -> String {
    let joined = match scopes {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    if joined.is_empty() {
        return ANTHROPIC_FALLBACK_SCOPES.to_string();
    }
    joined
}

/// Milliseconds since the Unix epoch, saturating at 0 if the host clock
/// is set before 1970.
fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Public entry point for an OpenAI (Codex/ChatGPT) OAuth-refresh
/// interception. Thin wrapper mirroring [`refresh_anthropic`]: binds the
/// host `codex` spawn, leaving the decisions to
/// [`refresh_openai_inner`].
fn refresh_openai(state_dir: &Path) -> Result<Vec<u8>> {
    let host_path = host_codex_auth_path().context("HOME not set")?;
    refresh_openai_inner(state_dir, &host_path, &|| {
        trigger_host_refresh(
            "codex",
            &[
                "exec",
                "--skip-git-repo-check",
                "--dangerously-bypass-approvals-and-sandbox",
                "Reply with OK",
            ],
            OPENAI_HOST_REFRESH_TIMEOUT,
        )
    })
}

/// [`refresh_openai`] with the host CLI behind a callback. Same shape,
/// and for the same reasons, as [`refresh_anthropic_inner`] — see there
/// for why the gate, the lock and the re-read are ordered this way.
///
/// The one thing that costs more here: the host CLI is `codex exec
/// --dangerously-bypass-approvals-and-sandbox`, an unsandboxed agentic
/// run on the *host* rather than one small inference call. It is the
/// highest-privilege thing this hook can do, and a guest HTTP request is
/// what triggers it — so the gate below is not only an optimization.
/// Before the host-expiry gate it ran on the first guest refresh after
/// every boot; now only within [`OPENAI_HOST_TOKEN_ROTATION_MARGIN`] of
/// a real expiry, and at most once per [`HOST_REFRESH_MIN_SPACING`].
fn refresh_openai_inner(
    state_dir: &Path,
    host_path: &Path,
    rotate_host: &dyn Fn() -> Result<()>,
) -> Result<Vec<u8>> {
    let read_host = || {
        std::fs::read_to_string(host_path)
            .with_context(|| format!("reading {}", host_path.display()))
    };
    let rotate = |raw: &str| {
        rotate_openai(state_dir, raw)
            .with_context(|| format!("rotating OpenAI token from {}", host_path.display()))
    };

    let raw = read_host()?;
    if !openai_host_token_needs_rotation(&raw) {
        return rotate(&raw);
    }

    // Single-flight, OpenAI-specific so an in-flight Anthropic refresh
    // doesn't serialize against this one.
    let _flight = RefreshLock::acquire(state_dir, secrets::REFRESH_LOCK_OPENAI)?;
    let raw = read_host()?;
    if openai_host_token_needs_rotation(&raw)
        && !host_refresh_damped(state_dir, secrets::REFRESH_STAMP_OPENAI)
    {
        note_host_refresh_attempt(state_dir, secrets::REFRESH_STAMP_OPENAI);
        if let Err(e) = rotate_host() {
            tracing::warn!(error = %format!("{e:#}"), "host OpenAI rotation failed");
            eprintln!("agent-vm: host OpenAI token rotation failed: {e:#}");
        }
        return rotate(&read_host()?);
    }
    rotate(&raw)
}

/// Pure rotation step for OpenAI: parse the (already-rotated) host
/// `codex auth.json` text, rewrite the per-project token file with the
/// fresh real access token, and synthesize the placeholder-carrying
/// OAuth refresh response. Split out from [`refresh_openai`] for the
/// same deterministic-testability reason as [`rotate_anthropic`].
fn rotate_openai(state_dir: &Path, host_auth_json: &str) -> Result<Vec<u8>> {
    let json: Value =
        serde_json::from_str(host_auth_json).context("parsing rotated host codex auth.json")?;

    // Same refusal as the Anthropic path, and it matters more here.
    // Codex ignores `expires_in` entirely and instead stamps
    // `last_refresh` in its own auth.json the moment it sees a 200 — so
    // a synthesized success against a dead host token doesn't just fail
    // the next request, it pushes the guest's *next* refresh attempt out
    // by codex's whole refresh interval (days), with no way back inside
    // the session.
    let (new_access, expires_in) = match choose_openai_credential(&json) {
        Ok(chosen) => chosen,
        Err(why) => return Ok(host_credentials_unusable_response(why, "codex login")),
    };

    let token_file = secrets::openai_token_path(state_dir);
    if let Some(parent) = token_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&token_file, new_access.as_bytes(), 0o600)?;

    let body = json!({
        "access_token": secrets::OPENAI_ACCESS_PLACEHOLDER,
        "refresh_token": secrets::OPENAI_REFRESH_PLACEHOLDER,
        "id_token": secrets::OPENAI_ID_PLACEHOLDER,
        "expires_in": expires_in,
        "token_type": "Bearer",
    });
    Ok(http_200_json(&serde_json::to_vec(&body)?))
}

/// How close to expiry the host ChatGPT access token has to be before a
/// `codex exec` is worth running.
///
/// Much tighter than [`HOST_TOKEN_ROTATION_MARGIN`] because codex is
/// much less eager than Claude Code: measured on 2026-08-11 against
/// codex-cli 0.146.1, a `codex exec` run with the host access token 8
/// hours from expiry (and `last_refresh` 9.8 days old) completed
/// normally and rotated *nothing* — same `last_refresh`, same token. So
/// a wide margin here does not buy an early rotation, it only buys
/// wasted agentic runs. Keep it just wide enough to cover the guest
/// asking a few minutes before the token actually dies.
const OPENAI_HOST_TOKEN_ROTATION_MARGIN: std::time::Duration =
    std::time::Duration::from_secs(300);

/// What an OpenAI credential with no readable expiry is reported as.
/// Codex ignores `expires_in`, so this only has to be a sane non-lie for
/// other readers of the response.
const OPENAI_UNKNOWN_EXPIRY_SECS: i64 = 3600;

/// True when the host ChatGPT access token is at (or near) expiry, so a
/// host-side `codex exec` might actually rotate it.
///
/// Conservative on uncertainty — an unparseable file errs towards the
/// spawn, since guessing wrong there merely costs a CLI run while
/// skipping a needed rotation hands the guest a dead bearer. Two cases
/// deliberately do *not* spawn, because nothing a CLI run could do would
/// help: a plain `OPENAI_API_KEY` login (API keys don't expire and are
/// never "rotated"), and a file with no credential in it at all (the
/// user is not logged in; `codex exec` cannot log them in, and the
/// answer is the refusal [`rotate_openai`] already produces).
///
/// Must stay in step with [`choose_openai_credential`]'s floor: whenever
/// this says a rotation is due and the rotation does not happen, that
/// one has to refuse. See the floor's own comment.
fn openai_host_token_needs_rotation(host_auth_json: &str) -> bool {
    let Ok(json) = serde_json::from_str::<Value>(host_auth_json) else {
        return true;
    };
    let Some(access) = non_empty_str(json.pointer("/tokens/access_token")) else {
        return false;
    };
    let Some(exp_ms) = jwt_exp_ms(access) else {
        return true;
    };
    exp_ms.saturating_sub(now_unix_ms()) <= OPENAI_HOST_TOKEN_ROTATION_MARGIN.as_millis() as i64
}

/// Pick the bearer to serve for OpenAI and how long it is good for, or
/// name why there is nothing to serve.
///
/// Order: the ChatGPT OAuth access token if it is usable, else a
/// configured API key, else refuse. The API-key fallback only fires for
/// a genuine API-key login — a ChatGPT-mode `auth.json` carries
/// `OPENAI_API_KEY: null` — so it cannot quietly serve the wrong kind of
/// credential to a ChatGPT-mode guest.
///
/// Asymmetric with [`openai_host_token_needs_rotation`] on purpose: that
/// one answers "is a spawn worth it", where guessing wrong costs a CLI
/// run; this answers "may we tell the guest it is refreshed", where
/// guessing wrong strands the session. So a token whose expiry we cannot
/// read is served (opaque tokens and API keys must keep working), while
/// one we can prove is dying is not.
fn choose_openai_credential(host_auth: &Value) -> Result<(&str, i64), &'static str> {
    /// Refuse anything at or under this. **Must be ≥
    /// [`OPENAI_HOST_TOKEN_ROTATION_MARGIN`]**, and that is the whole
    /// point: it makes the two decisions compose into "we judged a
    /// rotation due, and it did not happen, therefore refuse". A floor
    /// below the margin leaves a band where we declare a rotation due,
    /// fail to get one, and then serve the doomed token anyway — and
    /// because codex stamps `last_refresh` on our 200 and won't ask
    /// again for days, that band strands the session exactly as a
    /// fabricated expiry would.
    const FLOOR_SECS: i64 = OPENAI_HOST_TOKEN_ROTATION_MARGIN.as_secs() as i64;

    let api_key = non_empty_str(host_auth.get("OPENAI_API_KEY"));
    let Some(access) = non_empty_str(host_auth.pointer("/tokens/access_token")) else {
        return api_key
            .map(|k| (k, OPENAI_UNKNOWN_EXPIRY_SECS))
            .ok_or("the host codex auth has neither tokens.access_token nor OPENAI_API_KEY");
    };
    let Some(exp_ms) = jwt_exp_ms(access) else {
        // Opaque, non-JWT token: nothing we can prove, so serve it.
        return Ok((access, OPENAI_UNKNOWN_EXPIRY_SECS));
    };
    let remaining = exp_ms.saturating_sub(now_unix_ms()) / 1000;
    if remaining > FLOOR_SECS {
        return Ok((access, remaining));
    }
    // The OAuth token is spent. An API-key login can still be served.
    api_key
        .map(|k| (k, OPENAI_UNKNOWN_EXPIRY_SECS))
        .ok_or("the host ChatGPT access token has expired")
}

/// `Some` only for a JSON string with something in it.
fn non_empty_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(|v| v.as_str()).filter(|s| !s.is_empty())
}

/// Milliseconds-since-epoch of a JWT's `exp` claim, or `None` if the
/// string isn't a JWT we can read.
///
/// The signature is neither checked nor needed: this is the host's own
/// credential file, and we only want the expiry it already carries so we
/// can avoid handing the guest a token that is about to 401.
fn jwt_exp_ms(token: &str) -> Option<i64> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    // JWTs are base64url without padding; tolerate the standard
    // alphabet too (mirrors `secrets::decode_id_token_account`).
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.as_bytes())
        .or_else(|_| {
            let padded = format!(
                "{}{}",
                payload.replace('-', "+").replace('_', "/"),
                "=".repeat((4 - payload.len() % 4) % 4)
            );
            base64::engine::general_purpose::STANDARD.decode(padded.as_bytes())
        })
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    let exp = claims.get("exp")?;
    let secs = if exp.is_i64() || exp.is_u64() {
        exp.as_i64()?
    } else {
        exp.as_f64()? as i64
    };
    (secs > 0).then(|| secs.saturating_mul(1000))
}

/// Advisory cross-process lock serializing host-side OAuth refreshes
/// for one provider within a single project. Held for the duration of
/// one `refresh_anthropic` / `refresh_openai` call so two in-guest
/// agents (or two launchers) racing the *same* provider's token
/// rotation don't each spawn a host `claude -p` / `codex exec`.
///
/// The lock is keyed per provider (see [`secrets::refresh_lock_path_for`]):
/// an Anthropic rotation and an OpenAI rotation touch independent host
/// artifacts, so they hold different lock files and may run
/// concurrently — only same-provider refreshes serialize.
///
/// Uses a non-blocking `flock(LOCK_EX|LOCK_NB)` polled with a deadline
/// — already available via the `libc` dependency, so no new crate. The
/// lock is associated with the open file description and released
/// automatically when the fd is closed on `Drop` (or if the process
/// dies), so a crashed refresh can't wedge future rotations.
struct RefreshLock {
    /// `Some` when we actually hold the flock; `None` when [`acquire`]
    /// timed out waiting for a wedged-but-live holder and we degraded
    /// to proceeding without serialization (see [`acquire`]). The fd is
    /// still kept open in that case so `Drop` is uniform, but no
    /// `LOCK_UN` is issued.
    file: Option<std::fs::File>,
}

impl RefreshLock {
    /// Acquire the per-provider refresh lock named `lock_name` (e.g.
    /// [`secrets::REFRESH_LOCK_ANTHROPIC`]).
    ///
    /// Bounded wait ([`REFRESH_LOCK_TIMEOUT`]): a live-but-wedged holder
    /// must not block a waiter indefinitely, which would reintroduce the
    /// unbounded stall the host-CLI timeout caps were added to prevent
    /// (review #8).
    /// We poll `LOCK_EX|LOCK_NB` until the ceiling, then degrade to
    /// proceeding *without* the lock — i.e. the pre-feature behavior of
    /// just refreshing. That can cost a redundant host CLI spawn in the
    /// rare wedged-holder case, but keeps the whole refresh path time
    /// bounded, which matters more.
    fn acquire(state_dir: &Path, lock_name: &str) -> Result<Self> {
        Self::acquire_with_ceiling(state_dir, lock_name, REFRESH_LOCK_TIMEOUT)
    }

    fn acquire_with_ceiling(
        state_dir: &Path,
        lock_name: &str,
        ceiling: std::time::Duration,
    ) -> Result<Self> {
        let path = secrets::refresh_lock_path_for(state_dir, lock_name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating secrets dir {}", parent.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening refresh lock {}", path.display()))?;
        use std::os::unix::io::AsRawFd as _;
        use std::time::Instant;
        let fd = file.as_raw_fd();
        let start = Instant::now();
        // Poll interval is small relative to a host rotation (seconds);
        // the extra wakeups over the ceiling are negligible.
        let poll = std::time::Duration::from_millis(50);
        loop {
            let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                return Ok(Self { file: Some(file) });
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                // Held by someone else — wait and retry until the ceiling.
                Some(libc::EWOULDBLOCK) => {
                    if start.elapsed() >= ceiling {
                        // Degrade to no-lock rather than block forever on
                        // a wedged-but-live holder. `file: None` so Drop
                        // issues no LOCK_UN we never took.
                        tracing::warn!(
                            lock = %path.display(),
                            "refresh lock contended past {}s; proceeding without single-flight",
                            ceiling.as_secs(),
                        );
                        return Ok(Self { file: None });
                    }
                    std::thread::sleep(poll);
                    continue;
                }
                _ => {
                    return Err(anyhow::Error::new(err)
                        .context(format!("flock(LOCK_EX|LOCK_NB) on {}", path.display())));
                }
            }
        }
    }
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd as _;
        // Only unlock if we actually acquired it; a timed-out acquire
        // never took the lock, so issuing LOCK_UN would be wrong (and
        // could release a lock another fd in this process holds, though
        // that doesn't happen here).
        if let Some(file) = &self.file {
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

#[cfg(test)]
mod refresh_lock_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The spawn damper: no stamp means "go ahead", a just-written one
    /// means "someone already tried". (Replaces the old token-file mtime
    /// heuristic — the token file is rewritten on every refresh, including
    /// the ones that spawn nothing, so its mtime never answered this.)
    #[test]
    fn host_refresh_damper_window() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("proj");
        std::fs::create_dir_all(&state).unwrap();

        assert!(!host_refresh_damped(&state, secrets::REFRESH_STAMP_ANTHROPIC));
        note_host_refresh_attempt(&state, secrets::REFRESH_STAMP_ANTHROPIC);
        assert!(host_refresh_damped(&state, secrets::REFRESH_STAMP_ANTHROPIC));
        // Providers damp independently.
        assert!(!host_refresh_damped(&state, secrets::REFRESH_STAMP_OPENAI));

        let _ = std::fs::remove_dir_all(secrets::host_secret_dir_for_tests(&state));
    }

    /// Two threads contending the same project lock must run their
    /// critical sections one at a time. We track the number of holders
    /// inside the locked region and assert it never exceeds one.
    #[test]
    fn contending_threads_serialize() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("proj");
        std::fs::create_dir_all(&state).unwrap();

        let in_section = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let acquisitions = Arc::new(AtomicUsize::new(0));

        let spawn = |state: std::path::PathBuf,
                     in_section: Arc<AtomicUsize>,
                     max_concurrent: Arc<AtomicUsize>,
                     acquisitions: Arc<AtomicUsize>| {
            std::thread::spawn(move || {
                for _ in 0..20 {
                    let _guard = RefreshLock::acquire(&state, secrets::REFRESH_LOCK_ANTHROPIC)
                        .expect("acquire");
                    acquisitions.fetch_add(1, Ordering::SeqCst);
                    let now = in_section.fetch_add(1, Ordering::SeqCst) + 1;
                    // Record the peak observed concurrency.
                    max_concurrent.fetch_max(now, Ordering::SeqCst);
                    // Hold briefly to widen the race window.
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    in_section.fetch_sub(1, Ordering::SeqCst);
                }
            })
        };

        let t1 = spawn(
            state.clone(),
            in_section.clone(),
            max_concurrent.clone(),
            acquisitions.clone(),
        );
        let t2 = spawn(
            state.clone(),
            in_section.clone(),
            max_concurrent.clone(),
            acquisitions.clone(),
        );
        t1.join().unwrap();
        t2.join().unwrap();

        assert_eq!(acquisitions.load(Ordering::SeqCst), 40);
        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "flock failed to serialize: two holders entered the critical section at once"
        );
    }

    /// Different providers use different lock files, so a holder of the
    /// Anthropic lock must not block an OpenAI acquire (and vice versa).
    #[test]
    fn distinct_providers_do_not_contend() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("proj");
        std::fs::create_dir_all(&state).unwrap();

        let anthropic = RefreshLock::acquire(&state, secrets::REFRESH_LOCK_ANTHROPIC)
            .expect("anthropic acquire");
        // Holding the Anthropic lock, an OpenAI acquire must succeed
        // immediately (not time out, not degrade) — it's a different file.
        let openai = RefreshLock::acquire_with_ceiling(
            &state,
            secrets::REFRESH_LOCK_OPENAI,
            std::time::Duration::from_millis(50),
        )
        .expect("openai acquire");
        // Both genuinely hold their locks.
        assert!(openai.file.is_some(), "openai should hold its own lock");
        drop(anthropic);
        drop(openai);
    }

    /// A live-but-wedged holder must not block a waiter past the
    /// ceiling: the second acquire returns within the bound and degrades
    /// to the no-lock state instead of hanging forever.
    #[test]
    fn bounded_wait_degrades_when_holder_wedged() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("proj");
        std::fs::create_dir_all(&state).unwrap();

        // First holder keeps the lock for the whole test.
        let _held = RefreshLock::acquire(&state, secrets::REFRESH_LOCK_ANTHROPIC)
            .expect("first acquire");

        let ceiling = std::time::Duration::from_millis(200);
        let start = std::time::Instant::now();
        let second = RefreshLock::acquire_with_ceiling(
            &state,
            secrets::REFRESH_LOCK_ANTHROPIC,
            ceiling,
        )
        .expect("second acquire returns Ok (degraded)");
        let waited = start.elapsed();

        // Returned within a small multiple of the ceiling (not blocked
        // indefinitely), and degraded to no-lock.
        assert!(
            waited < ceiling * 4,
            "acquire blocked {waited:?}, expected ~{ceiling:?}"
        );
        assert!(
            second.file.is_none(),
            "contended acquire past ceiling must degrade to the no-lock state"
        );
    }
}

/// Bounds on how long we'll wait for a host CLI to drive a token
/// rotation. A hung host CLI must not keep the in-VM agent's OAuth
/// refresh waiting indefinitely (review #8).
///
/// **The whole hook must finish inside microsandbox's own 90 s
/// `HOOK_TIMEOUT`** (`crates/network/lib/intercept/handler.rs`), after
/// which msb kills us and the guest's refresh fails with a dropped
/// connection. These are therefore *shares* of that budget, not the
/// whole of it: worst case is [`REFRESH_LOCK_TIMEOUT`] waiting on a
/// contended lock, then the provider's timeout on the host CLI, then a
/// few file reads. 20 + 60 leaves headroom.
///
/// Before this split there was one 90 s constant used for both the lock
/// ceiling and the CLI, so a single contended refresh was guaranteed to
/// blow msb's budget.
///
/// The two providers get different budgets because the commands are not
/// comparable: `claude -p hi` is one small inference call (a few
/// seconds), while `codex exec` is a full agentic run. The Anthropic
/// side is additionally gated on
/// [`anthropic_host_token_needs_rotation`], so it only runs at all when
/// a rotation is genuinely due.
const ANTHROPIC_HOST_REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// See [`ANTHROPIC_HOST_REFRESH_TIMEOUT`]. Larger because `codex exec`
/// is a full agentic run, and because the OpenAI path has no
/// host-token-expiry gate yet, so it is spawned more often.
const OPENAI_HOST_REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Ceiling on waiting for the single-flight lock before degrading to an
/// unserialized refresh. Sized so lock wait + the slowest host CLI still
/// fits inside microsandbox's 90 s hook timeout; see
/// [`ANTHROPIC_HOST_REFRESH_TIMEOUT`].
const REFRESH_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// How much of a host CLI's stderr to keep for the error message. Past
/// this the tail is dropped — the message ends up in an msb log and in
/// the launcher's terminal, and the first few KiB carry the reason.
const HOST_CLI_STDERR_CAP: usize = 8 * 1024;

fn trigger_host_refresh(cmd: &str, args: &[&str], timeout: std::time::Duration) -> Result<()> {
    use std::time::{Duration, Instant};

    let mut child = Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        // Neither stream may be a pipe we don't service. We poll for
        // exit rather than blocking in `wait_with_output`, so a child
        // that fills the 64 KiB pipe buffer would block in `write()`
        // forever and burn the whole timeout budget — `codex exec` is a
        // full agentic run and streams plenty. stdout is never read at
        // all, so send it straight to /dev/null; stderr is wanted for
        // the error message, so it gets a reader thread below.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning host {cmd}"))?;

    // Drain stderr concurrently, keeping only the head, and hand the
    // result back over a channel. The thread ends on EOF — which needs
    // *every* holder of the write end to close it, and `codex exec`
    // runs arbitrary commands that inherit stderr, so an orphaned
    // grandchild can hold it open indefinitely. Hence a channel with a
    // deadline rather than a `join()`: the thread is left detached and
    // we move on without its output. Joining here would park the whole
    // hook past msb's own timeout — the exact stall the split budgets
    // exist to prevent.
    let pipe = child.stderr.take();
    let (tx, stderr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut kept: Vec<u8> = Vec::new();
        if let Some(mut pipe) = pipe {
            use std::io::Read as _;
            let mut chunk = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                // Keep reading past the cap (so the child never blocks
                // on a full pipe) but stop accumulating.
                let room = HOST_CLI_STDERR_CAP.saturating_sub(kept.len());
                if room > 0 {
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
        let _ = tx.send(kept);
    });
    /// How long to wait for the drained stderr once the child is gone.
    /// It is only used to make an error message better, so this is a
    /// courtesy, not a budget.
    const STDERR_GRACE: Duration = Duration::from_secs(1);
    let collect = |rx: &std::sync::mpsc::Receiver<Vec<u8>>| {
        String::from_utf8_lossy(&rx.recv_timeout(STDERR_GRACE).unwrap_or_default())
            .trim()
            .to_string()
    };

    let start = Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) => {
                let stderr = collect(&stderr_rx);
                if !status.success() {
                    anyhow::bail!("host {cmd} failed (status {status}): {stderr}");
                }
                return Ok(());
            }
            None => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let stderr = collect(&stderr_rx);
                    anyhow::bail!(
                        "host {cmd} did not return within {} s; killed: {stderr}",
                        timeout.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// True when the intercepted request really is a token-endpoint POST
/// for this provider.
///
/// Two things are checked, and both matter:
///
///  * **Method.** Only `POST` carries a grant.
///  * **Exact path.** Intercept rules match on a path *prefix*
///    (`find_matching_rule` upstream), so `POST /v1/oauth/token/revoke`
///    — what Claude Code sends on logout — lands on the same rule as a
///    refresh. Driving a host-side `claude -p` rotation (and answering
///    with a synthesized *token* response) for a revoke is wrong, so
///    anything but the exact endpoint is punted back as an error.
///
/// The target is normalised first: RFC 7230 absolute-form
/// (`POST https://platform.claude.com/v1/oauth/token`) is legal, and a
/// query string is stripped before comparing.
fn looks_like_oauth_refresh(req: &[u8], token_path: &str) -> bool {
    let Ok(text) = std::str::from_utf8(req) else {
        return false;
    };
    let Some(target) = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("POST "))
        .and_then(|rest| rest.split_whitespace().next())
    else {
        return false;
    };
    let path = normalize_origin_form(target);
    let path = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
    path == token_path
}

fn http_200_json(body: &[u8]) -> Vec<u8> {
    build_response(200, "OK", body)
}

/// Synthesized error handed back to the in-guest client.
///
/// The body uses `message`, not `error`: go-gh unmarshals only
/// `message` when it renders an `HTTPError`, so anything under another
/// key is silently dropped and the user sees a bare status code. The
/// whole point of denying rather than forwarding anonymously is that
/// the reason reaches the person reading the terminal.
fn error_response(code: u16, msg: &str) -> Vec<u8> {
    let body = format!("{{\"message\":{}}}", json!(msg));
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
        // Non-user-scoped utility surfaces: gh tooling friendly,
        // safe to forward with auth.
        let allowed = al(&[]);
        for path in ["/rate_limit", "/meta", "/markdown"] {
            assert!(
                matches!(github_access("POST", path, &allowed), GithubAccess::Authenticated)
                    || matches!(github_access("GET", path, &allowed), GithubAccess::Authenticated),
                "{path} should be Authenticated"
            );
        }
    }

    #[test]
    fn gh_access_graphql_path_is_anonymous_here() {
        // /graphql gets its own body-level filter before this
        // path-only policy runs; if it ever reaches github_access it
        // must NOT be granted the token on path alone.
        let allowed = al(&["wirenboard/agent-vm"]);
        for m in ["GET", "POST"] {
            assert_eq!(github_access(m, "/graphql", &allowed), GithubAccess::Anonymous);
        }
    }

    #[test]
    fn gh_access_search_and_notifications_are_anonymous() {
        // Per spec: third-party model. Authenticated search hits
        // private repos the user has access to → leaks private repo
        // inventory + code + issues. /notifications is inherently
        // user-scoped (your own notification feed) — third party
        // gets 401, which is the right answer.
        let allowed = al(&["wirenboard/agent-vm"]);
        for path in [
            "/search",
            "/search/code?q=foo+repo:any/repo",
            "/search/issues?q=is:open",
            "/search/repositories?q=stars:>1000",
            "/notifications",
            "/notifications/threads/123",
        ] {
            assert_eq!(
                github_access("GET", path, &allowed),
                GithubAccess::Anonymous,
                "{path} should be Anonymous (third-party access)"
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
        let r = format!(
            "POST /repos/x/y/issues HTTP/1.1\r\n\
             Host: api.github.com\r\n\
             Authorization: token {placeholder}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: 11\r\n\
             \r\n\
             {{\"title\":1}}",
            placeholder = secrets::GH_TOKEN_PLACEHOLDER,
        );
        let out = strip_authorization_from_request(r.as_bytes());
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
        assert!(!s.contains(secrets::GH_TOKEN_PLACEHOLDER));
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

    /// Regression: when Authorization is the LAST header (its line
    /// has no trailing \r\n in `head` — that \r\n is part of the
    /// \r\n\r\n separator in `rest`), an earlier implementation
    /// dropped the line but kept the previous header's trailing \r\n,
    /// and then appended `rest` which itself starts with \r\n\r\n.
    /// Result: three consecutive \r\n between headers and body,
    /// shifting body content by 2 bytes and breaking Content-Length
    /// or poisoning the next request on a keep-alive connection.
    #[test]
    fn strip_auth_when_authorization_is_last_header_no_extra_crlf() {
        let r = b"GET / HTTP/1.1\r\n\
                  Host: api.github.com\r\n\
                  Authorization: Bearer LAST\r\n\
                  \r\n\
                  body-bytes";
        let out = strip_authorization_from_request(r);
        let s = std::str::from_utf8(&out).unwrap();
        // Header/body separator must be exactly one \r\n\r\n.
        assert!(
            s.contains("Host: api.github.com\r\n\r\nbody-bytes"),
            "expected exactly one CRLFCRLF between headers and body; got:\n{s:?}"
        );
        assert!(!s.contains("\r\n\r\n\r\n"), "no triple CRLF; got:\n{s:?}");
        assert!(!s.to_ascii_lowercase().contains("authorization:"));
        // Body is preserved verbatim and starts immediately after the
        // single \r\n\r\n.
        assert!(s.ends_with("\r\n\r\nbody-bytes"));
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

    // ── set_connection_close ──────────────────────────────────────

    #[test]
    fn connection_close_injected_when_header_absent() {
        let r = b"GET / HTTP/1.1\r\nHost: github.com\r\n\r\n";
        let out = set_connection_close(r);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("Connection: close\r\n\r\n"), "got: {s:?}");
        assert!(s.starts_with("GET / HTTP/1.1\r\n"));
    }

    #[test]
    fn connection_close_replaces_existing_keep_alive() {
        // RFC 7230 says proxies must remove hop-by-hop headers
        // (Connection, Keep-Alive, Proxy-Connection) — we strip all
        // three and emit our own single Connection: close.
        let r = b"GET / HTTP/1.1\r\n\
                  Host: github.com\r\n\
                  Connection: keep-alive\r\n\
                  Keep-Alive: timeout=60\r\n\
                  Proxy-Connection: keep-alive\r\n\
                  \r\n";
        let out = set_connection_close(r);
        let s = std::str::from_utf8(&out).unwrap();
        // All three hop-by-hop headers must be removed; only our
        // single Connection: close remains.
        let lower = s.to_ascii_lowercase();
        assert_eq!(
            lower.matches("connection:").count(),
            1,
            "should have exactly one Connection header; got: {s:?}"
        );
        assert!(s.contains("Connection: close"));
        assert!(!lower.contains("keep-alive:"));
        assert!(!lower.contains("proxy-connection:"));
        assert!(s.contains("Host: github.com"));
    }

    #[test]
    fn connection_close_preserves_body_verbatim() {
        let r = b"POST /x HTTP/1.1\r\n\
                  Host: github.com\r\n\
                  Content-Length: 5\r\n\
                  \r\n\
                  hello";
        let out = set_connection_close(r);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("\r\n\r\nhello"), "body must follow exactly one \\r\\n\\r\\n; got: {s:?}");
        assert!(s.ends_with("hello"));
    }

    /// End-to-end: after the hook runs on a non-allow-listed clone,
    /// the bytes that reach upstream MUST contain `Connection: close`
    /// (to prevent the keep-alive bypass for any follow-up requests
    /// libcurl/git would do on the same TCP connection). This is the
    /// regression assertion for the bug behind real-world private
    /// repo clone-through.
    #[test]
    fn anonymous_passthrough_forces_connection_close() {
        use base64::Engine as _;
        use microsandbox_network::secrets::handler::SecretsHandler;

        let placeholder = secrets::GH_TOKEN_PLACEHOLDER;
        let real_token = "REAL_TOKEN_KEEPALIVE_DEFENSE_CANARY";

        let config = build_github_secrets_config(placeholder, real_token);
        let mut handler = SecretsHandler::new(&config, "github.com", true);

        let creds = format!("x-access-token:{placeholder}");
        let b64 = base64::engine::general_purpose::STANDARD.encode(creds.as_bytes());
        let req = format!(
            "GET /evgeny-boger/mitsubishi-ac-ir.git/info/refs?service=git-upload-pack HTTP/1.1\r\n\
             Host: github.com\r\n\
             Connection: keep-alive\r\n\
             User-Agent: git/2.47\r\n\
             Authorization: Basic {b64}\r\n\
             \r\n"
        );
        let substituted = handler.substitute(req.as_bytes()).expect("not a violation");
        let allowed: Vec<String> = Vec::new();
        assert!(matches!(
            github_smart_decision(&substituted, &allowed),
            GithubSmartOutcome::Anonymous
        ));

        // What the hook would write to stdout for Anonymous:
        let hook_out = set_connection_close(&strip_authorization_from_request(&substituted));
        let s = std::str::from_utf8(&hook_out).unwrap();

        let expected_creds = format!("x-access-token:{real_token}");
        let expected_b64 =
            base64::engine::general_purpose::STANDARD.encode(expected_creds.as_bytes());
        // No real token of any flavor.
        assert!(!s.contains(real_token), "raw real token leaked: {s}");
        assert!(!s.contains(&expected_b64), "base64 real token leaked: {s}");
        // No keep-alive — the defining property of the fix.
        assert!(
            s.contains("Connection: close"),
            "Connection: close missing — keep-alive bypass still possible: {s}"
        );
        let lower = s.to_ascii_lowercase();
        assert_eq!(lower.matches("connection:").count(), 1);
        assert!(!lower.contains("keep-alive:"));
    }

    /// End-to-end: same for the allow-listed (Authenticated) path.
    /// The real token IS allowed through (that's the point), but the
    /// connection still MUST be torn down after responding so that
    /// a subsequent (potentially non-allow-listed) request on the
    /// same TCP doesn't bypass the hook.
    #[test]
    fn authenticated_passthrough_forces_connection_close() {
        use base64::Engine as _;
        use microsandbox_network::secrets::handler::SecretsHandler;

        let placeholder = secrets::GH_TOKEN_PLACEHOLDER;
        let real_token = "REAL_TOKEN_ALLOWED_REPO";

        let config = build_github_secrets_config(placeholder, real_token);
        let mut handler = SecretsHandler::new(&config, "github.com", true);

        let creds = format!("x-access-token:{placeholder}");
        let b64 = base64::engine::general_purpose::STANDARD.encode(creds.as_bytes());
        let req = format!(
            "POST /wirenboard/agent-vm.git/git-upload-pack HTTP/1.1\r\n\
             Host: github.com\r\n\
             Connection: keep-alive\r\n\
             Authorization: Basic {b64}\r\n\
             Content-Length: 0\r\n\
             \r\n"
        );
        let substituted = handler.substitute(req.as_bytes()).expect("not a violation");
        let allowed = vec!["wirenboard/agent-vm".to_string()];
        assert!(matches!(
            github_smart_decision(&substituted, &allowed),
            GithubSmartOutcome::Authenticated
        ));

        let hook_out = set_connection_close(&substituted);
        let s = std::str::from_utf8(&hook_out).unwrap();

        // Real token IS in here — Authenticated means it's allowed.
        let expected_creds = format!("x-access-token:{real_token}");
        let expected_b64 =
            base64::engine::general_purpose::STANDARD.encode(expected_creds.as_bytes());
        assert!(s.contains(&expected_b64), "auth must reach upstream for allow-listed repo");
        // Connection must be close — the connection-reset is the
        // entire point even for the allowed path.
        assert!(
            s.contains("Connection: close"),
            "Connection: close missing on Authenticated: {s}"
        );
        let lower = s.to_ascii_lowercase();
        assert_eq!(lower.matches("connection:").count(), 1);
        assert!(!lower.contains("keep-alive:"));
    }

    // ── full pipeline (secrets → hook) end-to-end regression ──────
    //
    // These tests wire up the SAME pipeline order as
    // `vendor/microsandbox/.../tls/proxy.rs:forward_plaintext`:
    //
    //   1. SecretsHandler.substitute(guest_bytes)
    //         → inject_basic_auth base64-decodes the Authorization,
    //           swaps GH_TOKEN_PLACEHOLDER for the real token, re-encodes.
    //   2. github_smart_decision(substituted_bytes, allowed_repos)
    //         → Authenticated / Anonymous / Deny / Malformed.
    //   3. If Anonymous: strip_authorization_from_request(substituted_bytes)
    //         is the bytes the proxy actually writes to upstream.
    //
    // The invariant under test: for a NON-allow-listed private repo,
    // the bytes that would reach GitHub must NOT contain the real
    // token. If this test ever fails, a private repo clone would
    // succeed against the proxy.

    #[allow(dead_code)] // helper used only by the test below
    fn build_github_secrets_config(
        placeholder: &str,
        real_token: &str,
    ) -> microsandbox_network::secrets::config::SecretsConfig {
        use microsandbox_network::secrets::config::{
            HostPattern, SecretEntry, SecretInjection, SecretValue, SecretsConfig,
        };
        SecretsConfig {
            secrets: vec![SecretEntry {
                env_var: "GH_TOKEN".into(),
                value: SecretValue::Static(real_token.into()),
                placeholder: placeholder.into(),
                allowed_hosts: vec![
                    HostPattern::Exact("github.com".into()),
                    HostPattern::Exact("api.github.com".into()),
                    HostPattern::Exact("codeload.github.com".into()),
                    HostPattern::Exact("raw.github.com".into()),
                    HostPattern::Exact("objects.github.com".into()),
                ],
                injection: SecretInjection {
                    headers: true,
                    basic_auth: true,
                    query_params: false,
                    body: false,
                },
                on_violation: None,
                require_tls_identity: true,
            }],
            on_violation: Default::default(),
        }
    }

    /// E2E: git's first request to clone a private, NON-allow-listed
    /// repo. Pipeline: secrets-substitute(real token in Basic auth) →
    /// hook returns Anonymous (strip auth). Real token MUST NOT
    /// appear in the bytes the proxy would forward to GitHub.
    #[test]
    fn private_repo_clone_does_not_leak_real_token_through_pipeline() {
        use base64::Engine as _;
        use microsandbox_network::secrets::handler::SecretsHandler;

        // Sentinel values: if either string shows up in the final
        // upstream bytes, the test fails — we'd be leaking a real
        // token to the network.
        let placeholder = secrets::GH_TOKEN_PLACEHOLDER;
        let real_token = "REAL_TOKEN_MUST_NEVER_REACH_UPSTREAM_42";

        let config = build_github_secrets_config(placeholder, real_token);
        let mut handler = SecretsHandler::new(&config, "github.com", true);

        // What git actually sends when cloning a private repo via
        // HTTPS with the credential helper: Basic auth carrying
        // `x-access-token:<placeholder>` base64-encoded. Authorization
        // is the LAST header (the bug we fixed earlier hit exactly
        // this shape).
        let creds = format!("x-access-token:{placeholder}");
        let b64 = base64::engine::general_purpose::STANDARD.encode(creds.as_bytes());
        let request = format!(
            "GET /evgeny-boger/mitsubishi-ac-ir/info/refs?service=git-upload-pack HTTP/1.1\r\n\
             Host: github.com\r\n\
             User-Agent: git/2.47\r\n\
             Accept: */*\r\n\
             Authorization: Basic {b64}\r\n\
             \r\n"
        );
        let request_bytes = request.as_bytes();

        // Step 1: secrets layer substitutes the placeholder with the
        // real token (decoded basic creds, replaced, re-encoded).
        let substituted = handler.substitute(request_bytes).expect("not a violation");
        let substituted_bytes: &[u8] = &substituted;

        // Sanity check: the real token IS in the substituted bytes
        // (base64-encoded inside the Basic value). If this fails the
        // SecretsHandler isn't doing its job — different bug.
        let expected_creds = format!("x-access-token:{real_token}");
        let expected_b64 = base64::engine::general_purpose::STANDARD.encode(expected_creds.as_bytes());
        let substituted_str = std::str::from_utf8(substituted_bytes)
            .expect("substituted output should still be UTF-8 for this ASCII request");
        assert!(
            substituted_str.contains(&expected_b64),
            "secrets layer should have substituted the placeholder with the real token; \
             expected Basic value {expected_b64:?} in:\n{substituted_str}"
        );

        // Step 2: hook decides. Repo evgeny-boger/mitsubishi-ac-ir is
        // NOT in the (empty) allow-list → Anonymous.
        let allowed: Vec<String> = Vec::new();
        let decision = github_smart_decision(substituted_bytes, &allowed);
        assert!(
            matches!(decision, GithubSmartOutcome::Anonymous),
            "non-allow-listed repo must route to Anonymous (third-party access)"
        );

        // Step 3: stripped bytes are what hits upstream.
        let upstream_bytes = strip_authorization_from_request(substituted_bytes);
        let upstream_str = std::str::from_utf8(&upstream_bytes)
            .expect("stripped request should still be UTF-8");

        // INVARIANT: real token bytes (raw AND base64) must not be
        // anywhere in what we send upstream.
        assert!(
            !upstream_str.contains(real_token),
            "raw real token leaked to upstream:\n{upstream_str}"
        );
        assert!(
            !upstream_str.contains(&expected_b64),
            "base64-encoded real token leaked to upstream:\n{upstream_str}"
        );

        // And no Authorization header at all should reach upstream.
        assert!(
            !upstream_str.to_ascii_lowercase().contains("authorization:"),
            "Authorization header reached upstream:\n{upstream_str}"
        );
    }

    /// Two requests on the SAME connection (HTTP/1.1 keep-alive),
    /// which is what git+libcurl actually do during clone (info/refs
    /// then git-upload-pack). The SecretsHandler is created once per
    /// connection and reused for both requests. This test asserts
    /// that the substitution layer + naive hook-style filtering
    /// alone do NOT close the leak: a second request's real token
    /// can reach upstream if the hook isn't re-invoked.
    ///
    /// This is a hypothesis-confirmation test — it reproduces the
    /// keep-alive bypass that the unit-level pipeline test misses.
    /// If this test shows the second request's bytes contain the
    /// real token without going through the hook, we've found the
    /// real-world leak.
    #[test]
    fn keep_alive_second_request_bytes_contain_real_token_pre_hook() {
        use base64::Engine as _;
        use microsandbox_network::secrets::handler::SecretsHandler;

        let placeholder = secrets::GH_TOKEN_PLACEHOLDER;
        let real_token = "REAL_TOKEN_KEEPALIVE_LEAK_CANARY";

        let config = build_github_secrets_config(placeholder, real_token);
        // ONE handler per "connection".
        let mut handler = SecretsHandler::new(&config, "github.com", true);

        let creds = format!("x-access-token:{placeholder}");
        let b64 = base64::engine::general_purpose::STANDARD.encode(creds.as_bytes());

        // Request 1 — info/refs (the request the hook intercepts).
        let req1 = format!(
            "GET /evgeny-boger/mitsubishi-ac-ir.git/info/refs?service=git-upload-pack HTTP/1.1\r\n\
             Host: github.com\r\n\
             Authorization: Basic {b64}\r\n\
             \r\n"
        );
        let sub1 = handler.substitute(req1.as_bytes()).expect("not a violation");
        // The hook would run here and strip auth for non-allow-listed.
        // Asserting that part is the other test.

        // Request 2 — second request on the SAME connection (e.g.
        // a retry, or libcurl's pipelined follow-up). In the real
        // proxy, after the first dispatch the Interceptor goes to
        // State::Disabled and returns Verdict::Forward for every
        // subsequent chunk — which means the substituted bytes go
        // STRAIGHT to upstream, unfiltered.
        let req2 = format!(
            "POST /evgeny-boger/mitsubishi-ac-ir.git/git-upload-pack HTTP/1.1\r\n\
             Host: github.com\r\n\
             Content-Type: application/x-git-upload-pack-request\r\n\
             Authorization: Basic {b64}\r\n\
             Content-Length: 0\r\n\
             \r\n"
        );
        let sub2 = handler.substitute(req2.as_bytes()).expect("not a violation");
        let sub2_str = std::str::from_utf8(&sub2).unwrap();

        let expected_creds = format!("x-access-token:{real_token}");
        let expected_b64 = base64::engine::general_purpose::STANDARD.encode(expected_creds.as_bytes());

        // INVARIANT (currently FAILS for keep-alive): if the hook
        // isn't re-engaged for request 2, the substituted bytes
        // (with real token) are what hits the wire. This assertion
        // documents the leak — if it passes (i.e., real token IS
        // in sub2), we've confirmed the bypass.
        let leaked = sub2_str.contains(&expected_b64);
        assert!(
            leaked,
            "expected the keep-alive bypass to manifest: secret-substitution \
             puts the real token in the bytes of request 2 on the same connection, \
             and the proxy's interceptor goes to Disabled after request 1's \
             dispatch — so these bytes go upstream unfiltered. \
             If this assertion fails the leak may already be plugged."
        );

        // For completeness: prove request 1 alone is properly stripped by the hook.
        let allowed: Vec<String> = Vec::new();
        let decision1 = github_smart_decision(&sub1, &allowed);
        assert!(matches!(decision1, GithubSmartOutcome::Anonymous));
        let stripped1 = strip_authorization_from_request(&sub1);
        let stripped1_str = std::str::from_utf8(&stripped1).unwrap();
        assert!(
            !stripped1_str.contains(&expected_b64),
            "request 1 strip should remove the real token from the wire"
        );
    }

    /// E2E: same pipeline, but the repo IS allow-listed → hook
    /// returns Authenticated (empty stdout → proxy forwards the
    /// post-substitution bytes verbatim). Real token SHOULD appear
    /// upstream in this case (legitimate clone).
    #[test]
    fn allowlisted_repo_clone_does_pass_real_token_through_pipeline() {
        use base64::Engine as _;
        use microsandbox_network::secrets::handler::SecretsHandler;

        let placeholder = secrets::GH_TOKEN_PLACEHOLDER;
        let real_token = "REAL_TOKEN_FOR_ALLOWED_REPO";

        let config = build_github_secrets_config(placeholder, real_token);
        let mut handler = SecretsHandler::new(&config, "github.com", true);

        let creds = format!("x-access-token:{placeholder}");
        let b64 = base64::engine::general_purpose::STANDARD.encode(creds.as_bytes());
        let request = format!(
            "GET /wirenboard/agent-vm/info/refs?service=git-upload-pack HTTP/1.1\r\n\
             Host: github.com\r\n\
             Authorization: Basic {b64}\r\n\
             \r\n"
        );
        let substituted = handler.substitute(request.as_bytes()).expect("not a violation");

        let allowed = vec!["wirenboard/agent-vm".to_string()];
        let decision = github_smart_decision(&substituted, &allowed);
        assert!(
            matches!(decision, GithubSmartOutcome::Authenticated),
            "allow-listed repo must route to Authenticated"
        );

        // In the proxy, Authenticated means hook returns empty stdout
        // → `Verdict::ForwardBuffered(substituted)`. So the real token
        // (in the substituted bytes) IS what reaches upstream — this
        // is the intended path for legitimate auth on allowed repos.
        let upstream_str = std::str::from_utf8(&substituted).unwrap();
        let expected_creds = format!("x-access-token:{real_token}");
        let expected_b64 = base64::engine::general_purpose::STANDARD.encode(expected_creds.as_bytes());
        assert!(
            upstream_str.contains(&expected_b64),
            "for allow-listed repo, real token should reach upstream; got:\n{upstream_str}"
        );
    }
}

// ─── mid-session token-rotation regression tests (PLAN.md A1) ───────────
//
// The OAuth-refresh MITM exists but had never been exercised across a
// real token-expiry boundary — true e2e needs a long live session and a
// real expiring token, infeasible in CI / the dev sandbox. Instead we
// drive the rotation logic deterministically: `refresh_{anthropic,openai}`
// are thin wrappers that spawn the host CLI and read the rotated host
// credential file, then delegate to the pure `rotate_{anthropic,openai}`
// step. These tests call the pure step directly with a simulated rotated
// host file, then assert the two invariants that matter:
//
//   (1) the per-project token file is rewritten to the NEW real bearer
//       (so the proxy substitutes the fresh token on the next request);
//   (2) the synthesized HTTP refresh response carries only PLACEHOLDERS
//       in access_token / refresh_token — never the real bearer — and is
//       a well-formed HTTP/1.1 200 with the expected headers.
#[cfg(test)]
mod rotation_tests {
    use super::*;

    /// Minimal stdlib temp dir; avoids a dev-dependency. Unique per call
    /// via pid + a process-global counter, cleaned up on drop.
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let mut p = std::env::temp_dir();
            p.push(format!("agentvm-rot-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
            // Token files and stamps land in the *sibling* `<name>.secrets`
            // dir (that is the point — it is never bind-mounted into the
            // guest), so removing only the state dir leaves them behind.
            let _ = std::fs::remove_dir_all(secrets::host_secret_dir_for_tests(&self.0));
        }
    }

    /// Split a synthesized HTTP/1.1 response into (status_line, headers,
    /// body). Asserts a single CRLFCRLF separator exists.
    fn split_http(resp: &[u8]) -> (String, String, String) {
        let s = std::str::from_utf8(resp).expect("response is UTF-8");
        let sep = s.find("\r\n\r\n").expect("response has header/body separator");
        let head = &s[..sep];
        let body = &s[sep + 4..];
        let line_end = head.find("\r\n").unwrap_or(head.len());
        (
            head[..line_end].to_string(),
            head.to_string(),
            body.to_string(),
        )
    }

    // ── Anthropic ─────────────────────────────────────────────────

    const NEW_BEARER_ANTHROPIC: &str =
        "sk-ant-oat01-ROTATED-NEW-anthropic-bearer-value-do-not-leak";

    fn rotated_anthropic_creds() -> String {
        json!({
            "claudeAiOauth": {
                "accessToken": NEW_BEARER_ANTHROPIC,
                "refreshToken": "sk-ant-ort01-rotated-refresh",
                "expiresAt": 9_999_999_999_000i64,
                "scopes": ["user:inference", "user:profile"],
            }
        })
        .to_string()
    }

    #[test]
    fn anthropic_rotation_rewrites_token_file_to_new_bearer() {
        let tmp = TmpDir::new("anthropic-file");
        let resp = rotate_anthropic(tmp.path(), &rotated_anthropic_creds())
            .expect("rotate_anthropic should succeed");
        assert!(!resp.is_empty(), "response must not be empty");

        // (1) Per-project token file rewritten to the NEW real bearer.
        let token_file = secrets::anthropic_token_path(tmp.path());
        let written = std::fs::read_to_string(&token_file)
            .expect("token file should have been written");
        assert_eq!(
            written, NEW_BEARER_ANTHROPIC,
            "anthropic token file must hold the freshly-rotated real bearer"
        );
    }

    #[test]
    fn anthropic_rotation_response_carries_placeholders_not_real_bearer() {
        let tmp = TmpDir::new("anthropic-resp");
        let resp = rotate_anthropic(tmp.path(), &rotated_anthropic_creds())
            .expect("rotate_anthropic should succeed");
        let (status, headers, body) = split_http(&resp);

        // Well-formed status line + headers.
        assert_eq!(status, "HTTP/1.1 200 OK", "status line");
        assert!(
            headers.contains("Content-Type: application/json"),
            "Content-Type header present: {headers:?}"
        );
        assert!(
            headers.contains(&format!("Content-Length: {}", body.len())),
            "Content-Length matches body ({} bytes): {headers:?}",
            body.len()
        );
        assert!(
            headers.contains("Connection: close"),
            "Connection: close present: {headers:?}"
        );

        // (2) The real bearer must NEVER appear anywhere in the response.
        assert!(
            !String::from_utf8_lossy(&resp).contains(NEW_BEARER_ANTHROPIC),
            "real bearer leaked into the refresh response"
        );

        // Body's token fields are the placeholders, verbatim.
        let parsed: Value = serde_json::from_str(&body).expect("body is JSON");
        assert_eq!(
            parsed["access_token"], secrets::ANTHROPIC_ACCESS_PLACEHOLDER,
            "access_token must be the placeholder"
        );
        assert_eq!(
            parsed["refresh_token"], secrets::ANTHROPIC_REFRESH_PLACEHOLDER,
            "refresh_token must be the placeholder"
        );
        assert_eq!(parsed["token_type"], "Bearer");
        // expires_in derived from expiresAt: far-future → positive.
        assert!(
            parsed["expires_in"].as_i64().unwrap() > 0,
            "expires_in should be positive"
        );
    }

    // ── OpenAI / Codex ────────────────────────────────────────────

    const NEW_BEARER_OPENAI: &str =
        "eyJROTATED.openai.access.token.value.do.not.leak";

    fn rotated_openai_auth() -> String {
        json!({
            "tokens": {
                "access_token": NEW_BEARER_OPENAI,
                "refresh_token": "rotated-openai-refresh",
                "id_token": "rotated-openai-id",
            },
            "OPENAI_API_KEY": null,
        })
        .to_string()
    }

    #[test]
    fn openai_rotation_rewrites_token_file_to_new_bearer() {
        let tmp = TmpDir::new("openai-file");
        let resp = rotate_openai(tmp.path(), &rotated_openai_auth())
            .expect("rotate_openai should succeed");
        assert!(!resp.is_empty());

        let token_file = secrets::openai_token_path(tmp.path());
        let written = std::fs::read_to_string(&token_file)
            .expect("token file should have been written");
        assert_eq!(
            written, NEW_BEARER_OPENAI,
            "openai token file must hold the freshly-rotated real access token"
        );
    }

    #[test]
    fn openai_rotation_response_carries_placeholders_not_real_bearer() {
        let tmp = TmpDir::new("openai-resp");
        let resp = rotate_openai(tmp.path(), &rotated_openai_auth())
            .expect("rotate_openai should succeed");
        let (status, headers, body) = split_http(&resp);

        assert_eq!(status, "HTTP/1.1 200 OK", "status line");
        assert!(headers.contains("Content-Type: application/json"));
        assert!(headers.contains(&format!("Content-Length: {}", body.len())));
        assert!(headers.contains("Connection: close"));

        assert!(
            !String::from_utf8_lossy(&resp).contains(NEW_BEARER_OPENAI),
            "real access token leaked into the refresh response"
        );

        let parsed: Value = serde_json::from_str(&body).expect("body is JSON");
        assert_eq!(parsed["access_token"], secrets::OPENAI_ACCESS_PLACEHOLDER);
        assert_eq!(parsed["refresh_token"], secrets::OPENAI_REFRESH_PLACEHOLDER);
        assert_eq!(parsed["id_token"], secrets::OPENAI_ID_PLACEHOLDER);
        assert_eq!(parsed["token_type"], "Bearer");
    }

    /// The legacy ChatGPT/Codex shape stores the key flat as
    /// `OPENAI_API_KEY` (no `tokens` object). Rotation must pick it up.
    #[test]
    fn openai_rotation_falls_back_to_flat_api_key() {
        let tmp = TmpDir::new("openai-flat");
        let auth = json!({ "OPENAI_API_KEY": NEW_BEARER_OPENAI }).to_string();
        let resp = rotate_openai(tmp.path(), &auth).expect("rotate_openai should succeed");

        let token_file = secrets::openai_token_path(tmp.path());
        let written = std::fs::read_to_string(&token_file).unwrap();
        assert_eq!(written, NEW_BEARER_OPENAI);
        assert!(!String::from_utf8_lossy(&resp).contains(NEW_BEARER_OPENAI));
    }

    // ── OpenAI: the host token's real expiry ──────────────────────

    /// A ChatGPT access token shaped like the real thing: a JWT whose
    /// `exp` is `secs_from_now` away. Header and signature are inert —
    /// nothing verifies them, and nothing should.
    fn openai_jwt(secs_from_now: i64) -> String {
        use base64::Engine as _;
        let b64 = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s.as_bytes());
        let exp = now_unix_ms() / 1000 + secs_from_now;
        format!(
            "{}.{}.rotated-openai-signature",
            b64(r#"{"alg":"none","typ":"JWT"}"#),
            b64(&format!(r#"{{"exp":{exp},"chatgpt_account_id":"acct"}}"#)),
        )
    }

    fn openai_auth_expiring_in(secs_from_now: i64) -> String {
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "access_token": openai_jwt(secs_from_now),
                "refresh_token": "rotated-openai-refresh",
                "id_token": "rotated-openai-id",
                "account_id": "acct",
            },
            "last_refresh": "2026-08-01T19:52:44.003980550Z",
        })
        .to_string()
    }

    /// `expires_in` should be the access token's real remaining life.
    /// Codex ignores the field, but a truthful one costs nothing and the
    /// value is what tells us whether to refuse.
    #[test]
    fn openai_expires_in_tracks_the_access_token_jwt() {
        let auth: Value = serde_json::from_str(&openai_auth_expiring_in(4 * 3600)).unwrap();
        let (_, secs) = choose_openai_credential(&auth).expect("a live token is servable");
        assert!(
            (4 * 3600 - 60..=4 * 3600).contains(&secs),
            "expected ~4 h, got {secs}"
        );
    }

    /// The expensive half here is a full `codex exec` agentic run, and
    /// the host ChatGPT token lives for days — so the overwhelmingly
    /// common ask must not spawn one.
    #[test]
    fn openai_rotation_is_skipped_while_the_host_token_is_live() {
        assert!(!openai_host_token_needs_rotation(&openai_auth_expiring_in(
            4 * 24 * 3600
        )));
        assert!(!openai_host_token_needs_rotation(&openai_auth_expiring_in(
            3600
        )));
        // Inside the margin, and past it.
        assert!(openai_host_token_needs_rotation(&openai_auth_expiring_in(
            60
        )));
        assert!(openai_host_token_needs_rotation(&openai_auth_expiring_in(
            -60
        )));

        // Nothing a `codex exec` could fix must not spawn one: an
        // API-key login (keys don't rotate) or a file with no
        // credential at all (the user is simply not logged in, and the
        // answer is the refusal `rotate_openai` produces).
        assert!(!openai_host_token_needs_rotation(
            &json!({"OPENAI_API_KEY": "sk-test-key"}).to_string()
        ));
        assert!(!openai_host_token_needs_rotation("{}"));
        assert!(!openai_host_token_needs_rotation(
            &json!({"tokens": {}}).to_string()
        ));
        // A file we cannot parse might be a partial write; err towards
        // the spawn, which is only expensive.
        assert!(openai_host_token_needs_rotation("not json"));
        // An opaque token whose expiry we cannot read: same.
        assert!(openai_host_token_needs_rotation(
            &json!({"tokens": {"access_token": "opaque-not-a-jwt"}}).to_string()
        ));
    }

    /// The band between the two OpenAI decisions is where the danger
    /// lives: we judge a rotation due, codex declines to perform one,
    /// and if the floor were lower we would then serve a token that dies
    /// minutes later — after codex has stamped `last_refresh` and
    /// stopped asking for days. Serving floor and spawn margin must
    /// coincide so this refuses instead.
    #[test]
    fn openai_refuses_the_band_where_a_rotation_was_due_but_did_not_happen() {
        for secs in [61, 200, 299] {
            let auth = openai_auth_expiring_in(secs);
            assert!(
                openai_host_token_needs_rotation(&auth),
                "{secs}s: a rotation is due"
            );
            let tmp = TmpDir::new("openai-band");
            let (status, _, _) = split_http(&rotate_openai(tmp.path(), &auth).unwrap());
            assert_eq!(
                status, "HTTP/1.1 503 Service Unavailable",
                "{secs}s: a still-due rotation must refuse, not serve"
            );
        }
    }

    /// The invariant behind the case above, stated directly.
    #[test]
    fn openai_serving_floor_is_not_below_the_spawn_margin() {
        let margin = OPENAI_HOST_TOKEN_ROTATION_MARGIN.as_secs() as i64;
        // Just above the margin: no rotation due, and servable.
        let live = openai_auth_expiring_in(margin + 120);
        assert!(!openai_host_token_needs_rotation(&live));
        let auth: Value = serde_json::from_str(&live).unwrap();
        assert!(choose_openai_credential(&auth).is_ok());
    }

    /// An API-key login must keep working even when a stale ChatGPT
    /// token is still sitting in the same file.
    #[test]
    fn openai_falls_back_to_an_api_key_when_the_oauth_token_is_spent() {
        let mut auth: Value = serde_json::from_str(&openai_auth_expiring_in(-60)).unwrap();
        auth["OPENAI_API_KEY"] = json!("sk-test-key");
        let (bearer, _) = choose_openai_credential(&auth).expect("the API key is still usable");
        assert_eq!(bearer, "sk-test-key");
    }

    /// Codex stamps `last_refresh` the moment it sees a 200 and then
    /// won't ask again for days, so a synthesized success against a dead
    /// host token strands the session — refuse instead.
    #[test]
    fn openai_rotation_refuses_a_dead_host_token() {
        let tmp = TmpDir::new("openai-dead");
        let resp = rotate_openai(tmp.path(), &openai_auth_expiring_in(-60)).unwrap();
        let (status, _, body) = split_http(&resp);
        assert_eq!(status, "HTTP/1.1 503 Service Unavailable");
        assert!(body.contains("codex login"), "names the host-side fix: {body}");
        assert!(!body.contains("invalid_grant"));
        assert!(
            !String::from_utf8_lossy(&resp).contains(NEW_BEARER_OPENAI)
                && !body.contains("eyJ"),
            "a refusal must not carry any part of a real credential: {body}"
        );
        // A refusal must not leave a dead bearer in the token file.
        assert!(
            !secrets::openai_token_path(tmp.path()).exists(),
            "token file must not be written on a refusal"
        );
    }

    /// A credential we cannot read an expiry from (API key, opaque
    /// token) must still be served — refusing what we cannot prove is
    /// dead would break working setups.
    #[test]
    fn openai_rotation_serves_credentials_with_no_readable_expiry() {
        for auth in [
            json!({ "OPENAI_API_KEY": NEW_BEARER_OPENAI }).to_string(),
            json!({"tokens": {"access_token": "opaque-not-a-jwt"}}).to_string(),
        ] {
            let tmp = TmpDir::new("openai-opaque");
            let resp = rotate_openai(tmp.path(), &auth).unwrap();
            assert_eq!(split_http(&resp).0, "HTTP/1.1 200 OK");
        }
    }

    #[test]
    fn jwt_exp_is_read_from_both_base64_alphabets() {
        use base64::Engine as _;
        let payload = r#"{"exp":1900000000}"#;
        let url_safe = format!(
            "h.{}.s",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        );
        let standard = format!(
            "h.{}.s",
            base64::engine::general_purpose::STANDARD.encode(payload)
        );
        assert_eq!(jwt_exp_ms(&url_safe), Some(1_900_000_000_000));
        assert_eq!(jwt_exp_ms(&standard), Some(1_900_000_000_000));

        assert_eq!(jwt_exp_ms("not-a-jwt"), None);
        assert_eq!(jwt_exp_ms("h.bm90IGpzb24.s"), None);
        assert_eq!(jwt_exp_ms(secrets::OPENAI_ACCESS_PLACEHOLDER), None);
    }

    /// The OpenAI path gets the same ordering guarantees as the
    /// Anthropic one: no spawn while the token is live, one spawn when
    /// it is not, and the answer read back after the rotation.
    #[test]
    fn openai_host_cli_runs_only_when_the_token_is_at_expiry() {
        let live = TmpDir::new("openai-inner-live");
        let (path, cli) = host_cli(&live, &openai_auth_expiring_in(4 * 24 * 3600), None);
        let resp = refresh_openai_inner(live.path(), &path, &|| cli.run()).unwrap();
        assert_eq!(split_http(&resp).0, "HTTP/1.1 200 OK");
        assert_eq!(cli.calls.get(), 0, "a live host token needs no codex exec");

        let dead = TmpDir::new("openai-inner-dead");
        let (path, cli) = host_cli(
            &dead,
            &openai_auth_expiring_in(-60),
            Some(openai_auth_expiring_in(10 * 24 * 3600)),
        );
        let resp = refresh_openai_inner(dead.path(), &path, &|| cli.run()).unwrap();
        assert_eq!(
            split_http(&resp).0,
            "HTTP/1.1 200 OK",
            "the rotation should have fixed it"
        );
        assert_eq!(cli.calls.get(), 1);

        // Damped: a second ask inside the spacing does not spawn again.
        // (`host_cli` always writes `host-credentials.json`, so this
        // rewrites the same file rather than adding a second one.)
        let (path, cli2) = host_cli(&dead, &openai_auth_expiring_in(-60), None);
        let resp = refresh_openai_inner(dead.path(), &path, &|| cli2.run()).unwrap();
        assert_eq!(split_http(&resp).0, "HTTP/1.1 503 Service Unavailable");
        assert_eq!(cli2.calls.get(), 0, "the stamp must damp the repeat spawn");
    }

    // ── The scope field ───────────────────────────────────────────
    //
    // Regression cover for the bug that made every intercepted rotation
    // a silent no-op. See `oauth_scope_string` for the mechanism.

    /// Claude Code parses the refresh response's `scope` with
    /// `typeof scope !== "string" ? [] : scope.split(" ")`, then refuses
    /// to persist the refreshed credentials unless the result contains
    /// `user:inference`. So the field has to be a space-delimited
    /// string, not the JSON array the host credential file stores.
    #[test]
    fn anthropic_rotation_scope_is_a_space_delimited_string() {
        let tmp = TmpDir::new("anthropic-scope");
        let resp = rotate_anthropic(tmp.path(), &rotated_anthropic_creds())
            .expect("rotate_anthropic should succeed");
        let (_, _, body) = split_http(&resp);
        let parsed: Value = serde_json::from_str(&body).expect("body is JSON");

        let scope = parsed["scope"]
            .as_str()
            .expect("scope must be a STRING (an array parses to no scopes at all)");
        assert_eq!(scope, "user:inference user:profile");

        // The exact predicate Claude Code applies before saving.
        assert!(
            scope.split(' ').any(|s| s == "user:inference"),
            "scope must carry user:inference or the guest drops the refreshed expiry"
        );
    }

    /// Host `scopes` shapes we can be handed, and what each must render
    /// as. The empty cases must NOT produce an empty string — that
    /// reproduces the original bug by a different route.
    #[test]
    fn oauth_scope_string_renders_every_host_shape() {
        assert_eq!(
            oauth_scope_string(Some(&json!(["user:inference", "user:profile"]))),
            "user:inference user:profile",
            "array joins with spaces"
        );
        assert_eq!(
            oauth_scope_string(Some(&json!("user:inference user:profile"))),
            "user:inference user:profile",
            "an already-joined string passes through"
        );
        assert_eq!(
            oauth_scope_string(Some(&json!(["  user:inference  ", "", "user:profile"]))),
            "user:inference user:profile",
            "entries are trimmed and empties dropped"
        );
        for empty in [json!([]), json!(""), json!(null), json!(7)] {
            let rendered = oauth_scope_string(Some(&empty));
            assert_eq!(rendered, ANTHROPIC_FALLBACK_SCOPES, "fallback for {empty}");
            assert!(rendered.split(' ').any(|s| s == "user:inference"));
        }
        assert_eq!(oauth_scope_string(None), ANTHROPIC_FALLBACK_SCOPES);
    }

    // ── Deciding whether the host CLI is worth spawning ────────────

    fn creds_expiring_in(ms_from_now: i64) -> String {
        json!({
            "claudeAiOauth": {
                "accessToken": NEW_BEARER_ANTHROPIC,
                "expiresAt": now_unix_ms() + ms_from_now,
                "scopes": ["user:inference", "user:profile"],
            }
        })
        .to_string()
    }

    /// The host CLI can only rotate a token that is itself near expiry,
    /// so a guest refresh against a comfortably-live host token must not
    /// spawn one — that was a real inference round trip bought for
    /// nothing, on a path the guest hits routinely.
    #[test]
    fn host_rotation_is_skipped_while_the_host_token_is_live() {
        // Hours of life left: nothing for `claude -p` to do.
        assert!(!anthropic_host_token_needs_rotation(&creds_expiring_in(
            4 * 3_600_000
        )));
        // Just past our margin.
        assert!(!anthropic_host_token_needs_rotation(&creds_expiring_in(
            HOST_TOKEN_ROTATION_MARGIN.as_millis() as i64 + 60_000
        )));
        // Inside the margin, and already expired: rotate.
        assert!(anthropic_host_token_needs_rotation(&creds_expiring_in(
            HOST_TOKEN_ROTATION_MARGIN.as_millis() as i64 - 60_000
        )));
        assert!(anthropic_host_token_needs_rotation(&creds_expiring_in(
            -60_000
        )));
    }

    /// Our margin has to be wider than the 5 minutes Claude Code uses to
    /// decide *its* copy is expired, or the guest asks at the exact
    /// moment we decline and the two trade a pointless round trip.
    #[test]
    fn host_rotation_margin_exceeds_the_guest_refresh_window() {
        const GUEST_REFRESH_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);
        assert!(
            HOST_TOKEN_ROTATION_MARGIN > GUEST_REFRESH_WINDOW,
            "margin must clear the guest's own refresh window"
        );
    }

    /// Every uncertainty falls to "rotate": spawning the host CLI is
    /// merely expensive, whereas skipping a needed rotation hands the
    /// guest a bearer that 401s.
    #[test]
    fn host_rotation_decision_is_conservative_on_bad_input() {
        assert!(anthropic_host_token_needs_rotation("not json"));
        assert!(anthropic_host_token_needs_rotation("{}"));
        assert!(anthropic_host_token_needs_rotation(
            &json!({"claudeAiOauth": {"accessToken": "x"}}).to_string()
        ));
        assert!(anthropic_host_token_needs_rotation(
            &json!({"claudeAiOauth": {"expiresAt": 0}}).to_string()
        ));
    }

    // ── What we answer when the host token is unusable ────────────

    /// Every expiry the guest must not be handed. Each one used to end
    /// up as a fabricated `expires_in: 3600`, which — now that the guest
    /// actually persists our answer — buys 55 minutes of confidence in a
    /// bearer that 401s on first use.
    #[test]
    fn unusable_host_expiry_is_refused_not_papered_over() {
        let cases: Vec<(&str, String)> = vec![
            ("already expired", creds_expiring_in(-60_000)),
            (
                "inside the guest's own 5-minute refresh window",
                creds_expiring_in(120_000),
            ),
            (
                "missing expiresAt",
                json!({"claudeAiOauth": {"accessToken": NEW_BEARER_ANTHROPIC}}).to_string(),
            ),
            (
                "zero expiresAt",
                json!({"claudeAiOauth": {"accessToken": NEW_BEARER_ANTHROPIC, "expiresAt": 0}})
                    .to_string(),
            ),
            (
                "non-numeric expiresAt",
                json!({"claudeAiOauth": {"accessToken": NEW_BEARER_ANTHROPIC, "expiresAt": "soon"}})
                    .to_string(),
            ),
        ];
        for (what, creds) in cases {
            let tmp = TmpDir::new("anthropic-unusable");
            let resp = rotate_anthropic(tmp.path(), &creds).expect("must answer, not error");
            let (status, _, body) = split_http(&resp);
            assert_eq!(status, "HTTP/1.1 503 Service Unavailable", "{what}");

            let parsed: Value = serde_json::from_str(&body).expect("body is JSON");
            assert_eq!(parsed["error"], "temporarily_unavailable", "{what}");
            // NOT invalid_grant: Claude Code reacts to that by blanking
            // its stored credentials, which is destructive for what is
            // usually a transient host-side condition.
            assert!(
                !body.contains("invalid_grant"),
                "{what}: must not trigger the guest's dead-token path"
            );
            assert!(
                !body.contains(NEW_BEARER_ANTHROPIC),
                "{what}: real bearer leaked into the refusal"
            );
        }
    }

    /// A float `expiresAt` (`1.77e12`) is valid JSON and `as_i64()`
    /// silently misses it — which landed in the fabricated-3600 branch.
    #[test]
    fn float_expiry_is_read_as_a_real_expiry() {
        let far_future = (now_unix_ms() + 4 * 3_600_000) as f64;
        let creds = json!({
            "claudeAiOauth": {
                "accessToken": NEW_BEARER_ANTHROPIC,
                "expiresAt": far_future,
                "scopes": ["user:inference"],
            }
        })
        .to_string();
        let tmp = TmpDir::new("anthropic-float");
        let (status, _, body) = split_http(&rotate_anthropic(tmp.path(), &creds).unwrap());
        assert_eq!(status, "HTTP/1.1 200 OK");
        let expires_in: Value = serde_json::from_str(&body).unwrap();
        let expires_in = expires_in["expires_in"].as_i64().unwrap();
        assert!(
            (4 * 3600 - 60..=4 * 3600).contains(&expires_in),
            "expires_in should track the real expiry, got {expires_in}"
        );
    }

    /// `expires_in` must be the host token's real remaining life, so the
    /// guest's persisted expiry tracks the host's instead of drifting.
    #[test]
    fn expires_in_reports_the_real_remaining_life() {
        // A range, not an equality: the clock is read once here and
        // again inside, and the result is truncated to whole seconds.
        let hour = anthropic_expires_in(Some(&json!(now_unix_ms() + 3_600_000)))
            .expect("an hour of life is servable");
        assert!(
            (3595..=3600).contains(&hour),
            "expected ~3600 s, got {hour}"
        );
        // At/below the guest's own refresh window there is nothing worth
        // serving — the guest would treat it as expired on arrival.
        assert!(anthropic_expires_in(Some(&json!(now_unix_ms() + 300_000))).is_err());
        assert!(anthropic_expires_in(None).is_err());
    }

    // ── Ordering of the rotation decisions ────────────────────────

    /// Spawn recorder standing in for `claude -p hi`, so the decision
    /// order in `refresh_anthropic_inner` can be pinned without a live
    /// Claude session.
    struct HostCli {
        calls: std::cell::Cell<usize>,
        /// Credentials the "host CLI" leaves behind when it runs.
        rotates_to: Option<String>,
        path: PathBuf,
    }
    impl HostCli {
        fn run(&self) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            if let Some(new) = &self.rotates_to {
                std::fs::write(&self.path, new)?;
            }
            Ok(())
        }
    }

    fn host_cli(dir: &TmpDir, initial: &str, rotates_to: Option<String>) -> (PathBuf, HostCli) {
        let path = dir.path().join("host-credentials.json");
        std::fs::write(&path, initial).unwrap();
        let cli = HostCli {
            calls: std::cell::Cell::new(0),
            rotates_to,
            path: path.clone(),
        };
        (path, cli)
    }

    /// The expensive half of a refresh is the host inference call, and
    /// the guest asks far more often than the host token needs rotating.
    #[test]
    fn a_live_host_token_is_served_without_spawning_the_host_cli() {
        let tmp = TmpDir::new("inner-live");
        let (path, cli) = host_cli(&tmp, &creds_expiring_in(4 * 3_600_000), None);

        let resp = refresh_anthropic_inner(tmp.path(), &path, &|| cli.run()).unwrap();
        let (status, _, _) = split_http(&resp);
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert_eq!(cli.calls.get(), 0, "no rotation was due; nothing to spawn");
        // The token file is still re-synced from the host file — that is
        // the whole point of answering rather than declining.
        assert_eq!(
            std::fs::read_to_string(secrets::anthropic_token_path(tmp.path())).unwrap(),
            NEW_BEARER_ANTHROPIC
        );
    }

    /// A host token at expiry does get the host CLI, and the answer is
    /// built from what the CLI left behind, not from the pre-rotation
    /// read.
    #[test]
    fn an_expiring_host_token_spawns_the_host_cli_once_and_reads_the_result() {
        let tmp = TmpDir::new("inner-rotate");
        let (path, cli) = host_cli(
            &tmp,
            &creds_expiring_in(60_000),
            Some(creds_expiring_in(4 * 3_600_000)),
        );

        let resp = refresh_anthropic_inner(tmp.path(), &path, &|| cli.run()).unwrap();
        let (status, _, body) = split_http(&resp);
        assert_eq!(status, "HTTP/1.1 200 OK", "rotation should have fixed it");
        assert_eq!(cli.calls.get(), 1);
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert!(
            parsed["expires_in"].as_i64().unwrap() > 3600,
            "expires_in must come from the post-rotation read"
        );
    }

    /// A host CLI that exits 0 without actually rotating must not be
    /// spawned again on the guest's next ask — otherwise every guest
    /// re-triggers a real inference call for as long as the condition
    /// lasts.
    #[test]
    fn a_host_cli_that_does_not_rotate_is_not_spawned_again_immediately() {
        let tmp = TmpDir::new("inner-nooprotate");
        // Enough life to still be served, little enough to stay inside
        // the rotation margin — the band where the guest keeps asking.
        let (path, cli) = host_cli(&tmp, &creds_expiring_in(480_000), None);

        for _ in 0..3 {
            let resp = refresh_anthropic_inner(tmp.path(), &path, &|| cli.run()).unwrap();
            assert_eq!(split_http(&resp).0, "HTTP/1.1 200 OK");
        }
        assert_eq!(
            cli.calls.get(),
            1,
            "the attempt stamp must damp repeat spawns"
        );

        // ...and it is a *window*, not a latch: once the spacing has
        // passed, a retry is allowed again. (Asserting this is what
        // separates the damper from "never rotate twice".)
        let stamp =
            secrets::host_refresh_stamp_path(tmp.path(), secrets::REFRESH_STAMP_ANTHROPIC);
        let past = std::time::SystemTime::now() - HOST_REFRESH_MIN_SPACING * 2;
        std::fs::File::open(&stamp)
            .unwrap()
            .set_modified(past)
            .unwrap();
        refresh_anthropic_inner(tmp.path(), &path, &|| cli.run()).unwrap();
        assert_eq!(cli.calls.get(), 2, "a spawn is allowed once the spacing elapses");
    }

    /// A host CLI that outright fails must not drop the connection under
    /// the guest when the host token is in fact still usable — the guest
    /// may only be asking because its own copy is stale.
    #[test]
    fn a_failing_host_cli_still_serves_a_usable_host_token() {
        let tmp = TmpDir::new("inner-clifail");
        let (path, _) = host_cli(&tmp, &creds_expiring_in(480_000), None);
        let calls = std::cell::Cell::new(0);

        let resp = refresh_anthropic_inner(tmp.path(), &path, &|| {
            calls.set(calls.get() + 1);
            anyhow::bail!("host claude failed (status exit status: 1)")
        })
        .expect("a failed host CLI must not error the hook out");
        assert_eq!(calls.get(), 1);
        assert_eq!(split_http(&resp).0, "HTTP/1.1 200 OK");
    }

    /// `claude -p` refreshes the token *before* its inference call, so
    /// the routine non-zero exits (overloaded, rate limited, out of
    /// credit) come back with the rotation already done. Answering from
    /// the pre-spawn read there would write the *old* bearer into the
    /// token file and refuse a refresh that actually succeeded.
    #[test]
    fn a_failing_host_cli_that_still_rotated_is_answered_from_the_new_file() {
        let tmp = TmpDir::new("inner-clifail-rotated");
        let (path, cli) = host_cli(
            &tmp,
            &creds_expiring_in(60_000),
            Some(creds_expiring_in(4 * 3_600_000)),
        );

        let resp = refresh_anthropic_inner(tmp.path(), &path, &|| {
            cli.run().unwrap();
            anyhow::bail!("host claude failed (status exit status: 1)")
        })
        .expect("must still answer");
        let (status, _, body) = split_http(&resp);
        assert_eq!(status, "HTTP/1.1 200 OK", "the rotation did happen");
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert!(parsed["expires_in"].as_i64().unwrap() > 3600);
        assert_eq!(
            std::fs::read_to_string(secrets::anthropic_token_path(tmp.path())).unwrap(),
            NEW_BEARER_ANTHROPIC
        );
    }

    /// …but when the host token is *not* usable either, the failure is
    /// reported rather than dressed up.
    #[test]
    fn a_failing_host_cli_on_a_dead_token_is_reported() {
        let tmp = TmpDir::new("inner-clifail-dead");
        let (path, _) = host_cli(&tmp, &creds_expiring_in(-60_000), None);

        let resp = refresh_anthropic_inner(tmp.path(), &path, &|| anyhow::bail!("nope"))
            .expect("still an HTTP answer, not a dropped connection");
        assert_eq!(split_http(&resp).0, "HTTP/1.1 503 Service Unavailable");
    }

    // ── Budget ────────────────────────────────────────────────────

    /// The whole hook — lock wait plus host CLI — has to finish inside
    /// microsandbox's own hook timeout, or msb kills us mid-rotation and
    /// the guest's refresh dies with a dropped connection.
    ///
    /// Reads the bound out of the vendored microsandbox source rather
    /// than restating it, so an upstream change to `HOOK_TIMEOUT` fails
    /// here instead of in production.
    #[test]
    fn refresh_budget_fits_inside_the_msb_hook_timeout() {
        const MSB_HANDLER_SRC: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../vendor/microsandbox/crates/network/lib/intercept/handler.rs"
        ));
        let secs: u64 = MSB_HANDLER_SRC
            .lines()
            .find_map(|l| {
                let l = l.trim();
                let rest = l.strip_prefix("const HOOK_TIMEOUT")?;
                let open = rest.find("from_secs(")? + "from_secs(".len();
                let close = rest[open..].find(')')?;
                rest[open..open + close].trim().parse().ok()
            })
            .expect("could not read HOOK_TIMEOUT out of the vendored intercept handler");
        let msb_hook_timeout = std::time::Duration::from_secs(secs);

        for (provider, budget) in [
            ("anthropic", ANTHROPIC_HOST_REFRESH_TIMEOUT),
            ("openai", OPENAI_HOST_REFRESH_TIMEOUT),
        ] {
            assert!(
                budget + REFRESH_LOCK_TIMEOUT < msb_hook_timeout,
                "{provider}: lock wait + host CLI ({:?}) must leave headroom \
                 under msb's {msb_hook_timeout:?}",
                budget + REFRESH_LOCK_TIMEOUT
            );
        }
    }

    // ── Which requests count as a refresh ─────────────────────────

    /// Intercept rules match on a path *prefix*, so the logout endpoint
    /// `/v1/oauth/token/revoke` reaches this hook too. Only the exact
    /// token endpoint may drive a host-side rotation.
    #[test]
    fn only_the_exact_token_endpoint_is_treated_as_a_refresh() {
        let post = |target: &str| {
            format!("POST {target} HTTP/1.1\r\nHost: platform.claude.com\r\n\r\n{{}}").into_bytes()
        };
        let path = secrets::ANTHROPIC_OAUTH_TOKEN_PATH;

        assert!(looks_like_oauth_refresh(&post(path), path));
        assert!(
            looks_like_oauth_refresh(&post(&format!("{path}?x=1")), path),
            "a query string is not part of the path"
        );
        assert!(
            looks_like_oauth_refresh(&post(&format!("https://platform.claude.com{path}")), path),
            "RFC 7230 absolute-form is legal and must still match"
        );

        assert!(
            !looks_like_oauth_refresh(&post(&format!("{path}/revoke")), path),
            "logout must not drive a token rotation"
        );
        assert!(!looks_like_oauth_refresh(&post("/v1/oauth/hello"), path));
        assert!(
            !looks_like_oauth_refresh(&post(&format!("{path}/")), path),
            "a trailing slash is a different endpoint"
        );
        assert!(
            !looks_like_oauth_refresh(
                b"GET /v1/oauth/token HTTP/1.1\r\nHost: platform.claude.com\r\n\r\n",
                path
            ),
            "only POST carries a grant"
        );
        assert!(!looks_like_oauth_refresh(b"", path));
        assert!(!looks_like_oauth_refresh(&[0xff, 0xfe, 0xfd], path));

        // The OpenAI endpoint sits at a different path, and the two must
        // not accept each other's.
        let openai = secrets::OPENAI_OAUTH_TOKEN_PATH;
        assert!(looks_like_oauth_refresh(&post(openai), openai));
        assert!(!looks_like_oauth_refresh(&post(openai), path));
        assert!(!looks_like_oauth_refresh(&post(path), openai));
    }

    /// A host file we cannot parse at all is an internal error: it
    /// travels up to `run`, which turns it into the same refusal
    /// response rather than dropping the connection.
    #[test]
    fn rotation_errors_on_unparseable_host_file() {
        let tmp = TmpDir::new("malformed");
        assert!(rotate_anthropic(tmp.path(), "not json").is_err());
        assert!(rotate_openai(tmp.path(), "not json").is_err());
        assert!(
            rotate_anthropic(tmp.path(), &json!({"claudeAiOauth": {}}).to_string()).is_err(),
            "missing accessToken must error"
        );
    }

    /// A *parseable* file with no credential in it is not an internal
    /// error — it is the ordinary "not logged in on the host" case, and
    /// it earns the refusal response with a reason rather than an error
    /// exit that drops the connection under the guest.
    #[test]
    fn rotation_refuses_a_host_file_with_no_credential() {
        let tmp = TmpDir::new("no-cred");
        let resp = rotate_openai(tmp.path(), &json!({"tokens": {}}).to_string())
            .expect("a missing credential is answered, not errored");
        let (status, _, body) = split_http(&resp);
        assert_eq!(status, "HTTP/1.1 503 Service Unavailable");
        assert!(body.contains("codex login"), "names the host-side fix: {body}");
        assert!(
            !secrets::openai_token_path(tmp.path()).exists(),
            "nothing to write, so nothing must be written"
        );
    }
}
