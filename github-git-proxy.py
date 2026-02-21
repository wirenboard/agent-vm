#!/usr/bin/env python3
"""
Host-side Git HTTP proxy for Claude Code VMs.

Accepts HTTP Git requests from the VM, injects the GitHub token for the
configured repo only, and forwards to github.com over HTTPS.

Requests targeting other repos are forwarded without credentials (they'll
fail auth on GitHub's side rather than being blocked by the proxy).

The VM never sees the GitHub token.

Usage:
    GITHUB_MCP_TOKEN=ghu_... GITHUB_MCP_OWNER=wirenboard GITHUB_MCP_REPO=agent-vm \
        python3 github-git-proxy.py
    # Prints the listening port to stdout.
    # Then in the VM:
    #   git push http://host.lima.internal:PORT/wirenboard/agent-vm.git main

Optional env vars:
    GITHUB_GIT_PROXY_DEBUG    Set to "1" for verbose logging
    GITHUB_GIT_PROXY_LOG_DIR  Directory for log file (default: current dir)
"""

import base64
import http.client
import http.server
import os
import signal
import ssl
import sys
import socketserver
import time

TOKEN = os.environ.get("GITHUB_TOKEN", os.environ.get("GITHUB_MCP_TOKEN", ""))
OWNER = os.environ.get("GITHUB_MCP_OWNER", "")
REPO = os.environ.get("GITHUB_MCP_REPO", "")
DEBUG = os.environ.get("GITHUB_GIT_PROXY_DEBUG", "0") == "1"
LOG_DIR = os.environ.get("GITHUB_GIT_PROXY_LOG_DIR", ".")

# Retry settings for transient upstream errors (5xx)
MAX_RETRIES = 3
RETRY_DELAYS = [1, 2, 4]

# GitHub git HTTP uses Basic auth with x-access-token as username
_BASIC_AUTH = "Basic " + base64.b64encode(f"x-access-token:{TOKEN}".encode()).decode()

# Path prefixes that get credentials injected
_AUTHED_PREFIXES = []
if OWNER and REPO:
    _AUTHED_PREFIXES = [
        f"/{OWNER}/{REPO}.git/",
        f"/{OWNER}/{REPO}/",
    ]

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

    def _is_scoped_repo(self):
        """Return True if the request path matches the configured repo."""
        if not _AUTHED_PREFIXES:
            return False
        path = self.path.split("?")[0] + "/"
        return any(path.startswith(p) for p in _AUTHED_PREFIXES)

    def proxy(self):
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length) if content_length else None

        inject_auth = self._is_scoped_repo()
        debug(f">>> {self.command} {self.path} ({content_length} bytes, auth={'yes' if inject_auth else 'no'})")

        # Build upstream headers
        headers = {}
        for key, value in self.headers.items():
            if key.lower() in ("host", "authorization"):
                continue
            headers[key] = value
        headers["Host"] = "github.com"
        if inject_auth:
            headers["Authorization"] = _BASIC_AUTH
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


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    allow_reuse_address = True
    daemon_threads = True


def main():
    if not TOKEN:
        print("Error: GITHUB_TOKEN (or GITHUB_MCP_TOKEN) required", file=sys.stderr)
        sys.exit(1)

    _open_log()

    server = Server(("127.0.0.1", 0), GitProxyHandler)
    port = server.server_address[1]
    print(port, flush=True)

    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    signal.signal(signal.SIGINT, lambda *_: sys.exit(0))

    log(f"Listening on port {port}, forwarding to https://github.com")
    if OWNER and REPO:
        log(f"Token injected for: {OWNER}/{REPO}")
    else:
        log(f"WARNING: no repo scope configured, no auth injected")
    server.serve_forever()


if __name__ == "__main__":
    main()
