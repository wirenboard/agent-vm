#!/usr/bin/env python3
"""
Host-side GitHub MCP proxy for Claude Code VMs.

Accepts unauthenticated MCP requests from the VM, injects the GitHub token
as a Bearer header, enforces repo scope, and forwards to GitHub's hosted
MCP endpoint at api.githubcopilot.com/mcp/.

The VM never sees the GitHub token.

Usage:
    GITHUB_MCP_TOKEN=ghu_... GITHUB_MCP_OWNER=wirenboard GITHUB_MCP_REPO=agent-vm \
        python3 github-mcp-proxy.py
    # Prints the listening port to stdout, then serves until SIGTERM.

Optional env vars for tool filtering (server-side, via GitHub MCP headers):
    GITHUB_MCP_TOOLSETS   Comma-separated toolsets (default: "repos,issues,pull_requests,git,labels")
    GITHUB_MCP_TOOLS      Comma-separated tool names (e.g. "get_file_contents,issue_read")
    GITHUB_MCP_READONLY   Set to "1" for read-only mode
    GITHUB_MCP_LOCKDOWN   Set to "0" to disable lockdown mode (enabled by default)
"""

import http.client
import http.server
import json
import os
import re
import signal
import ssl
import sys
import socketserver
import time

MCP_HOST = "api.githubcopilot.com"
MCP_PORT = 443
MCP_PATH = "/mcp/"

# Retry settings for transient upstream errors (5xx)
MAX_RETRIES = 3           # Total attempts = MAX_RETRIES + 1
RETRY_DELAYS = [1, 2, 4]  # Seconds between retries (exponential backoff)
ERROR_LOG_DIR = os.environ.get("GITHUB_MCP_PROXY_LOG_DIR", ".")

TOKEN = os.environ.get("GITHUB_MCP_TOKEN", "")
OWNER = os.environ.get("GITHUB_MCP_OWNER", "")
REPO = os.environ.get("GITHUB_MCP_REPO", "")
DEBUG = os.environ.get("GITHUB_MCP_PROXY_DEBUG", "0") == "1"

# Comma-separated toolsets to expose via X-MCP-Toolsets header.
# Default is limited to toolsets that make sense for a single-repo-scoped token.
# Excluded by default: actions, code_security, dependabot, discussions, gists,
# notifications, orgs, projects, secret_protection, security_advisories,
# stargazers, users, copilot, copilot_spaces.
# See https://github.com/github/github-mcp-server/blob/main/docs/remote-server.md
DEFAULT_TOOLSETS = "repos,issues,pull_requests,git,labels"
TOOLSETS = os.environ.get("GITHUB_MCP_TOOLSETS", DEFAULT_TOOLSETS)

# Optional: comma-separated list of individual tool names to expose (e.g.
# "get_file_contents,issue_read").  Sent as X-MCP-Tools header.
# When empty, all tools within the allowed toolsets are available.
TOOLS = os.environ.get("GITHUB_MCP_TOOLS", "")

# Optional: set to "1" to restrict to read-only operations.
READONLY = os.environ.get("GITHUB_MCP_READONLY", "0") == "1"

# Lockdown mode: hides public issue details from users without push access.
# Enabled by default for security.
LOCKDOWN = os.environ.get("GITHUB_MCP_LOCKDOWN", "1") != "0"

# MCP tool argument fields that specify a repo target
REPO_FIELDS = {
    "owner": OWNER,
    "repo": REPO,
}

# Tools that are safe without repo scoping (read-only user metadata)
UNSCOPED_TOOLS = {"get_me"}

# Search tools that use a "query" parameter instead of owner/repo.
# We inject "repo:OWNER/REPO" to enforce scope.
SEARCH_TOOLS = {
    "search_code",
    "search_repositories",
    "search_issues",
    "search_pull_requests",
}

# Search tools that are blocked entirely because repo: scoping doesn't
# apply to their domain (users aren't scoped to a repo).
BLOCKED_SEARCH_TOOLS = {"search_users", "search_orgs"}

# Tools that use org-level fields instead of owner/repo
ORG_TOOLS = {"get_teams", "get_team_members", "list_issue_types"}

# All known tools and their categories.  Unknown tools are blocked by default
# to prevent new upstream tools from bypassing scope checks.
KNOWN_TOOLS = (
    UNSCOPED_TOOLS
    | SEARCH_TOOLS
    | BLOCKED_SEARCH_TOOLS
    | ORG_TOOLS
    # Tools with owner/repo fields (checked by REPO_FIELDS loop)
    | {
        # repos toolset
        "create_branch", "create_or_update_file", "create_repository",
        "delete_file", "fork_repository", "get_commit", "get_file_contents",
        "get_latest_release", "get_release_by_tag", "get_tag",
        "list_branches", "list_commits", "list_releases", "list_tags",
        "push_files",
        # issues toolset
        "add_issue_comment", "assign_copilot_to_issue", "get_label",
        "issue_read", "issue_write", "list_issues", "sub_issue_write",
        # pull_requests toolset
        "add_comment_to_pending_review", "add_reply_to_pull_request_comment",
        "create_pull_request", "list_pull_requests", "merge_pull_request",
        "pull_request_read", "pull_request_review_write",
        "request_copilot_review", "update_pull_request",
        "update_pull_request_branch",
        # git toolset
        "get_repository_tree",
        # labels toolset
        "label_write", "list_label",
    }
)


def debug(msg):
    if DEBUG:
        print(f"[github-mcp-proxy {time.strftime('%H:%M:%S')}] {msg}", file=sys.stderr)


def _looks_like_html(body_bytes, headers):
    """Check if a response body is HTML (not a normal JSON API error)."""
    for k, v in headers:
        if k.lower() == "content-type" and "html" in v.lower():
            return True
    if body_bytes:
        prefix = body_bytes[:256].lstrip()
        if prefix.startswith((b"<", b"<!DOCTYPE", b"<!doctype")):
            return True
    return False


def _save_error_response(status, body_bytes, context=""):
    """Save an error response body to a file for debugging. Returns the path."""
    ts = time.strftime("%Y%m%d-%H%M%S")
    filename = f"github-mcp-error-{ts}-{status}.html"
    path = os.path.join(ERROR_LOG_DIR, filename)
    try:
        with open(path, "wb") as f:
            if context:
                f.write(f"<!-- {context} -->\n".encode())
            f.write(body_bytes)
        debug(f"  saved error response to {path}")
    except OSError as e:
        debug(f"  failed to save error response: {e}")
        path = None
    return path


def _enforce_search_scope(tool, args):
    """For search tools, inject repo scope into the query string.

    Returns (modified_args, error_message).
    """
    if not OWNER or not REPO:
        return args, None

    query = args.get("query", "")
    scope = f"repo:{OWNER}/{REPO}"

    # Reject repo: qualifiers that point to a different repo.
    for match in re.finditer(r'\brepo:(\S+)', query):
        if match.group(1) != f"{OWNER}/{REPO}":
            msg = (f"Repo scope violation: {tool} query contains "
                   f"repo:{match.group(1)}, expected repo:{OWNER}/{REPO}")
            debug(f"  BLOCKED: {msg}")
            return None, msg

    # Reject org: and user: qualifiers — these widen search beyond the
    # scoped repo and could leak cross-repo data.
    for match in re.finditer(r'\b(org|user):(\S+)', query):
        qualifier, value = match.group(1), match.group(2)
        msg = (f"Repo scope violation: {tool} query contains "
               f"{qualifier}:{value} (not allowed, use repo: scope)")
        debug(f"  BLOCKED: {msg}")
        return None, msg

    # If no repo: qualifier present, inject one
    if f"repo:{OWNER}/{REPO}" not in query:
        args = dict(args)
        args["query"] = f"{scope} {query}".strip()
        debug(f"  injected scope: {args['query']}")

    return args, None


def enforce_repo_scope(body_bytes):
    """Parse MCP request body, enforce owner/repo in tool call arguments.

    Returns (modified_body_bytes, error_message). error_message is None on success.
    """
    if not body_bytes:
        return body_bytes, None

    try:
        req = json.loads(body_bytes)
    except (json.JSONDecodeError, UnicodeDecodeError):
        return body_bytes, None

    method = req.get("method")
    if method != "tools/call":
        return body_bytes, None

    params = req.get("params", {})
    tool = params.get("name", "unknown")
    args = params.get("arguments", {})
    if not isinstance(args, dict):
        return body_bytes, None

    # Block unknown tools (default-deny). New upstream tools won't
    # silently bypass scope checks.
    if tool not in KNOWN_TOOLS:
        msg = f"Repo scope violation: unknown tool {tool!r} is not allowed"
        debug(f"  BLOCKED: {msg}")
        return None, msg

    # Allow safe tools that don't need repo scoping
    if tool in UNSCOPED_TOOLS:
        debug(f"  allowed unscoped tool: {tool}")
        return body_bytes, None

    # Block org-level tools and user/org search tools
    if tool in ORG_TOOLS or tool in BLOCKED_SEARCH_TOOLS:
        msg = f"Repo scope violation: {tool} is not allowed (not repo-scoped)"
        debug(f"  BLOCKED: {msg}")
        return None, msg

    # For search tools, inject repo scope into the query
    if tool in SEARCH_TOOLS:
        args, err = _enforce_search_scope(tool, args)
        if err:
            return None, err
        req["params"]["arguments"] = args
        return json.dumps(req).encode(), None

    # For tools with owner/repo, enforce scoped values and inject if missing
    modified = False
    for field, enforced_value in REPO_FIELDS.items():
        if enforced_value:
            if field in args:
                if args[field] != enforced_value:
                    msg = f"Repo scope violation: {tool} called with {field}={args[field]!r}, expected {enforced_value!r}"
                    debug(f"  BLOCKED: {msg}")
                    return None, msg
            else:
                args[field] = enforced_value
                modified = True
                debug(f"  injected {field}={enforced_value!r} for {tool}")

    if modified:
        req["params"]["arguments"] = args
        return json.dumps(req).encode(), None

    return body_bytes, None


class GitHubMCPProxyHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):
        pass

    def do_request(self):
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length) if content_length else None

        debug(f">>> {self.command} {self.path} ({content_length} bytes)")

        # Enforce repo scope on tool calls
        if body and self.command == "POST":
            body, err = enforce_repo_scope(body)
            if err:
                self.send_error_response(403, err)
                return

        # Build upstream headers: copy all, replace auth and host
        headers = {}
        for key, value in self.headers.items():
            lower = key.lower()
            if lower in ("authorization", "host",
                         "x-mcp-toolsets", "x-mcp-tools",
                         "x-mcp-readonly", "x-mcp-lockdown"):
                continue
            headers[key] = value

        headers["Authorization"] = f"Bearer {TOKEN}"
        headers["Host"] = MCP_HOST
        if body:
            headers["Content-Length"] = str(len(body))

        # Inject tool-filtering headers (set by host, never from VM)
        if TOOLSETS:
            headers["X-MCP-Toolsets"] = TOOLSETS
        if TOOLS:
            headers["X-MCP-Tools"] = TOOLS
        if READONLY:
            headers["X-MCP-Readonly"] = "true"
        if LOCKDOWN:
            headers["X-MCP-Lockdown"] = "true"

        # Forward to GitHub's MCP endpoint, with retry on 5xx errors
        ctx = ssl.create_default_context()
        upstream_path = MCP_PATH + self.path.lstrip("/")
        upstream_path = upstream_path.replace("//", "/")

        last_error = None
        for attempt in range(MAX_RETRIES + 1):
            if attempt > 0:
                delay = RETRY_DELAYS[min(attempt - 1, len(RETRY_DELAYS) - 1)]
                debug(f"  retry {attempt}/{MAX_RETRIES} after {delay}s")
                time.sleep(delay)

            conn = None
            try:
                t0 = time.monotonic()
                conn = http.client.HTTPSConnection(MCP_HOST, MCP_PORT, context=ctx, timeout=300)
                conn.request(self.command, upstream_path, body=body, headers=headers)
                upstream = conn.getresponse()
                latency_ms = (time.monotonic() - t0) * 1000
            except Exception as e:
                if conn:
                    conn.close()
                debug(f"  UPSTREAM ERROR (attempt {attempt + 1}): {e}")
                last_error = str(e)
                continue

            upstream_headers = upstream.getheaders()
            is_streaming = any(
                k.lower() == "transfer-encoding" for k, v in upstream_headers
            )

            debug(f"<<< {upstream.status} {'streaming' if is_streaming else 'complete'} ({latency_ms:.0f}ms)")

            # On 5xx with HTML body, save the error and retry
            if upstream.status >= 500 and not is_streaming:
                body_data = upstream.read()
                conn.close()
                if _looks_like_html(body_data, upstream_headers):
                    ctx_msg = f"HTTP {upstream.status} from {MCP_HOST}{upstream_path} attempt {attempt + 1}"
                    _save_error_response(upstream.status, body_data, ctx_msg)
                    last_error = f"GitHub returned HTTP {upstream.status} (HTML error page)"
                    debug(f"  got HTML error page ({len(body_data)} bytes), will retry")
                    continue
                else:
                    # Non-HTML 5xx (JSON error) — still retry
                    last_error = f"GitHub returned HTTP {upstream.status}"
                    debug(f"  got {upstream.status} ({len(body_data)} bytes), will retry")
                    continue

            # Success or non-5xx error — forward response
            if is_streaming:
                self.send_response(upstream.status)
                for key, value in upstream_headers:
                    lower = key.lower()
                    if lower in ("transfer-encoding", "content-length",
                                 "connection", "keep-alive"):
                        continue
                    self.send_header(key, value)
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                total_bytes = 0
                try:
                    while True:
                        data = upstream.read(8192)
                        if not data:
                            break
                        total_bytes += len(data)
                        self.wfile.write(f"{len(data):x}\r\n".encode())
                        self.wfile.write(data)
                        self.wfile.write(b"\r\n")
                        self.wfile.flush()
                    self.wfile.write(b"0\r\n\r\n")
                    self.wfile.flush()
                    debug(f"  streamed {total_bytes} bytes")
                except (BrokenPipeError, ConnectionResetError) as e:
                    debug(f"  stream broken: {e} after {total_bytes} bytes")
                finally:
                    conn.close()
            else:
                body_data = upstream.read()
                conn.close()
                debug(f"  body: {len(body_data)} bytes")
                if DEBUG and len(body_data) < 4096:
                    try:
                        debug(f"  {body_data.decode()}")
                    except UnicodeDecodeError:
                        pass
                self.send_response(upstream.status)
                for key, value in upstream_headers:
                    lower = key.lower()
                    if lower in ("transfer-encoding", "content-length",
                                 "connection", "keep-alive"):
                        continue
                    self.send_header(key, value)
                self.send_header("Content-Length", str(len(body_data)))
                self.end_headers()
                self.wfile.write(body_data)
                self.wfile.flush()
            return

        # All retries exhausted
        debug(f"  all {MAX_RETRIES + 1} attempts failed: {last_error}")
        self.send_error_response(502, f"GitHub MCP unavailable after {MAX_RETRIES + 1} attempts: {last_error}")

    def send_error_response(self, code, message):
        body = json.dumps({"error": {"type": "proxy_error", "message": message}}).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    do_GET = do_request
    do_POST = do_request
    do_PUT = do_request
    do_DELETE = do_request


class QuietServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    allow_reuse_address = True
    daemon_threads = True


def main():
    if not TOKEN:
        print("Error: GITHUB_MCP_TOKEN is required", file=sys.stderr)
        sys.exit(1)
    if not OWNER or not REPO:
        print("Error: GITHUB_MCP_OWNER and GITHUB_MCP_REPO are required", file=sys.stderr)
        sys.exit(1)

    server = QuietServer(("127.0.0.1", 0), GitHubMCPProxyHandler)
    port = server.server_address[1]

    print(port, flush=True)

    def handle_signal(signum, frame):
        server.shutdown()

    signal.signal(signal.SIGTERM, handle_signal)
    signal.signal(signal.SIGINT, handle_signal)

    debug(f"Listening on port {port}, forwarding to {MCP_HOST}{MCP_PATH}")
    debug(f"Repo scope: {OWNER}/{REPO}")
    if TOOLSETS:
        debug(f"Toolsets: {TOOLSETS}")
    if TOOLS:
        debug(f"Tools: {TOOLS}")
    if READONLY:
        debug(f"Read-only mode: enabled")
    if LOCKDOWN:
        debug(f"Lockdown mode: enabled")
    server.serve_forever()


if __name__ == "__main__":
    main()
