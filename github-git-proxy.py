#!/usr/bin/env python3
"""
Host-side Git HTTP proxy for Claude Code VMs.

Accepts HTTP Git requests from the VM, injects the GitHub token for
configured repos, and forwards to github.com over HTTPS.

Requests targeting other repos are forwarded without credentials.

The VM never sees the GitHub token.

Usage (multi-repo, preferred):
    GITHUB_GIT_PROXY_REPOS='{"wirenboard/agent-vm":"ghu_xxx","wirenboard/wb-utils":"ghu_yyy"}' \
        python3 github-git-proxy.py

Usage (single-repo, backward compat):
    GITHUB_MCP_TOKEN=ghu_... GITHUB_MCP_OWNER=wirenboard GITHUB_MCP_REPO=agent-vm \
        python3 github-git-proxy.py

Optional env vars:
    GITHUB_GIT_PROXY_DEBUG    Set to "1" for verbose logging
    GITHUB_GIT_PROXY_LOG_DIR  Directory for log file (default: current dir)
"""

import base64
import http.client
import http.server
import json
import os
import signal
import ssl
import sys
import socketserver
import time
from urllib.parse import urlparse

DEBUG = os.environ.get("GITHUB_GIT_PROXY_DEBUG", "0") == "1"
LOG_DIR = os.environ.get("GITHUB_GIT_PROXY_LOG_DIR", ".")

# Retry settings for transient upstream errors (5xx)
MAX_RETRIES = 3
RETRY_DELAYS = [1, 2, 4]

# Build per-repo auth: { "/owner/repo.git/": "Basic ...", "/owner/repo/": "Basic ...", ... }
_REPO_AUTH = {}
# Bearer token for API requests (populated from the first repo's token)
_DEFAULT_BEARER = None


def _make_basic_auth(token):
    return "Basic " + base64.b64encode(f"x-access-token:{token}".encode()).decode()


def _add_repo_auth(owner, repo, token):
    global _DEFAULT_BEARER
    auth = _make_basic_auth(token)
    _REPO_AUTH[f"/{owner}/{repo}.git/"] = auth
    _REPO_AUTH[f"/{owner}/{repo}/"] = auth
    if _DEFAULT_BEARER is None:
        _DEFAULT_BEARER = token


# Preferred: multi-repo JSON config
_repos_json = os.environ.get("GITHUB_GIT_PROXY_REPOS", "")
if _repos_json:
    for slug, token in json.loads(_repos_json).items():
        owner, repo = slug.split("/", 1)
        _add_repo_auth(owner, repo, token)
else:
    # Backward compat: single-repo env vars
    _TOKEN = os.environ.get("GITHUB_TOKEN", os.environ.get("GITHUB_MCP_TOKEN", ""))
    _OWNER = os.environ.get("GITHUB_MCP_OWNER", "")
    _REPO = os.environ.get("GITHUB_MCP_REPO", "")
    if _TOKEN and _OWNER and _REPO:
        _add_repo_auth(_OWNER, _REPO, _TOKEN)

_log_file = None


def _open_log():
    global _log_file
    path = os.path.join(LOG_DIR, "github-git-proxy.log")
    _log_file = open(path, "a")


def log(msg):
    if _log_file:
        _log_file.write(f"[github-git-proxy {time.strftime('%H:%M:%S')}] {msg}\n")
        _log_file.flush()


def debug(msg):
    if DEBUG:
        log(msg)


def _looks_like_html(body_bytes):
    """Check if a response body is HTML."""
    if body_bytes:
        prefix = body_bytes[:256].lstrip()
        if prefix.startswith((b"<", b"<!DOCTYPE", b"<!doctype")):
            return True
    return False


def _save_error_response(status, body_bytes, context=""):
    """Save an error response body to a file for debugging. Returns the path."""
    ts = time.strftime("%Y%m%d-%H%M%S")
    filename = f"github-git-error-{ts}-{status}.html"
    path = os.path.join(LOG_DIR, filename)
    try:
        with open(path, "wb") as f:
            if context:
                f.write(f"<!-- {context} -->\n".encode())
            f.write(body_bytes)
        log(f"  saved error response to {path}")
    except OSError as e:
        log(f"  failed to save error response: {e}")
        path = None
    return path


class GitProxyHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):
        debug(format % args)

    def _match_repo_auth(self):
        """Return the Basic auth header if the request path matches a configured repo, else None."""
        if not _REPO_AUTH:
            return None
        path = self.path.split("?")[0] + "/"
        for prefix, auth in _REPO_AUTH.items():
            if path.startswith(prefix):
                return auth
        return None

    def _is_api_request(self):
        """Detect API requests from gh CLI via HTTP_PROXY.

        When HTTP_PROXY is set, Go sends the full URL in the request line:
          GET http://api.github.localhost/repos/... HTTP/1.1
        The Host header will contain 'api.github.localhost' or similar.
        Also support /api/ prefix for direct proxy access.
        """
        host = self.headers.get("Host", "")
        return host.startswith("api.") or self.path.startswith("/api/")

    def _proxy_api(self):
        """Forward an API request to api.github.com with bearer auth."""
        if not _DEFAULT_BEARER:
            self.send_error(502, "No GitHub token configured for API requests")
            return

        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length) if content_length else None

        # Extract the path from a proxy-style full URL or a direct path.
        # Full URL: http://api.github.localhost/repos/...  -> /repos/...
        # Direct:   /api/v3/repos/...                      -> /repos/...
        path = self.path
        if path.startswith("http://") or path.startswith("https://"):
            # Proxy-style: extract path portion
            path = urlparse(path).path
        # Strip /api/v3 prefix (GHE-style URLs that gh sometimes uses)
        if path.startswith("/api/v3/"):
            path = path[len("/api/v3"):]
        elif path.startswith("/api/"):
            path = path[len("/api"):]

        debug(f">>> API {self.command} {path} ({content_length} bytes)")

        # Build upstream headers
        headers = {}
        for key, value in self.headers.items():
            if key.lower() in ("host", "authorization", "accept-encoding"):
                continue
            headers[key] = value
        headers["Host"] = "api.github.com"
        headers["Authorization"] = f"token {_DEFAULT_BEARER}"
        if body:
            headers["Content-Length"] = str(len(body))

        ctx = ssl.create_default_context()
        last_error = None
        for attempt in range(MAX_RETRIES + 1):
            if attempt > 0:
                delay = RETRY_DELAYS[min(attempt - 1, len(RETRY_DELAYS) - 1)]
                log(f"  API retry {attempt}/{MAX_RETRIES} after {delay}s")
                time.sleep(delay)

            conn = None
            try:
                t0 = time.monotonic()
                conn = http.client.HTTPSConnection("api.github.com", 443, context=ctx, timeout=300)
                conn.request(self.command, path, body=body, headers=headers)
                resp = conn.getresponse()
                latency_ms = (time.monotonic() - t0) * 1000
            except Exception as e:
                if conn:
                    conn.close()
                log(f"  API UPSTREAM ERROR (attempt {attempt + 1}): {e}")
                last_error = str(e)
                continue

            resp_body = resp.read()
            conn.close()
            debug(f"<<< API {resp.status} ({latency_ms:.0f}ms, {len(resp_body)} bytes)")

            if resp.status >= 500:
                last_error = f"GitHub API returned HTTP {resp.status}"
                log(f"  API got {resp.status} ({len(resp_body)} bytes), will retry")
                continue

            # Forward response
            self.send_response(resp.status)
            for key, value in resp.getheaders():
                if key.lower() in ("transfer-encoding", "connection", "keep-alive"):
                    continue
                self.send_header(key, value)
            self.send_header("Content-Length", str(len(resp_body)))
            self.end_headers()
            self.wfile.write(resp_body)
            return

        log(f"  API all {MAX_RETRIES + 1} attempts failed: {last_error}")
        self.send_error(502, f"GitHub API unavailable after {MAX_RETRIES + 1} attempts: {last_error}")

    def proxy(self):
        # Route API requests before git path matching
        if self._is_api_request():
            return self._proxy_api()

        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length) if content_length else None

        matched_auth = self._match_repo_auth()
        debug(f">>> {self.command} {self.path} ({content_length} bytes, auth={'yes' if matched_auth else 'no'})")

        # Build upstream headers
        headers = {}
        for key, value in self.headers.items():
            if key.lower() in ("host", "authorization"):
                continue
            headers[key] = value
        headers["Host"] = "github.com"
        if matched_auth:
            headers["Authorization"] = matched_auth
        if body:
            headers["Content-Length"] = str(len(body))

        ctx = ssl.create_default_context()
        last_error = None
        for attempt in range(MAX_RETRIES + 1):
            if attempt > 0:
                delay = RETRY_DELAYS[min(attempt - 1, len(RETRY_DELAYS) - 1)]
                log(f"  retry {attempt}/{MAX_RETRIES} after {delay}s")
                time.sleep(delay)

            conn = None
            try:
                t0 = time.monotonic()
                conn = http.client.HTTPSConnection("github.com", 443, context=ctx, timeout=300)
                conn.request(self.command, self.path, body=body, headers=headers)
                resp = conn.getresponse()
                latency_ms = (time.monotonic() - t0) * 1000
            except Exception as e:
                if conn:
                    conn.close()
                log(f"  UPSTREAM ERROR (attempt {attempt + 1}): {e}")
                last_error = str(e)
                continue

            resp_body = resp.read()
            conn.close()
            debug(f"<<< {resp.status} ({latency_ms:.0f}ms, {len(resp_body)} bytes)")

            # On 5xx with HTML body, save the error and retry
            if resp.status >= 500:
                if _looks_like_html(resp_body):
                    ctx_msg = f"HTTP {resp.status} from github.com{self.path} attempt {attempt + 1}"
                    _save_error_response(resp.status, resp_body, ctx_msg)
                    last_error = f"GitHub returned HTTP {resp.status} (HTML error page)"
                    log(f"  got HTML error page ({len(resp_body)} bytes), will retry")
                else:
                    last_error = f"GitHub returned HTTP {resp.status}"
                    log(f"  got {resp.status} ({len(resp_body)} bytes), will retry")
                continue

            # Success or non-5xx — forward response
            self.send_response(resp.status)
            for key, value in resp.getheaders():
                if key.lower() in ("transfer-encoding", "connection", "keep-alive"):
                    continue
                self.send_header(key, value)
            self.send_header("Content-Length", str(len(resp_body)))
            self.end_headers()
            self.wfile.write(resp_body)
            return

        # All retries exhausted
        log(f"  all {MAX_RETRIES + 1} attempts failed: {last_error}")
        self.send_error(502, f"GitHub unavailable after {MAX_RETRIES + 1} attempts: {last_error}")

    do_GET = proxy
    do_POST = proxy
    do_PATCH = proxy
    do_PUT = proxy
    do_DELETE = proxy


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    allow_reuse_address = True
    daemon_threads = True


def main():
    if not _REPO_AUTH:
        print("Error: set GITHUB_GIT_PROXY_REPOS or GITHUB_TOKEN+GITHUB_MCP_OWNER+GITHUB_MCP_REPO", file=sys.stderr)
        sys.exit(1)

    _open_log()

    server = Server(("127.0.0.1", 0), GitProxyHandler)
    port = server.server_address[1]
    print(port, flush=True)

    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    signal.signal(signal.SIGINT, lambda *_: sys.exit(0))

    # Log configured repos (without tokens)
    repo_slugs = [p.strip("/").removesuffix(".git") for p in _REPO_AUTH if p.endswith(".git/")]
    log(f"Listening on port {port}, forwarding to https://github.com")
    log(f"Token injected for: {', '.join(repo_slugs)}")
    server.serve_forever()


if __name__ == "__main__":
    main()
