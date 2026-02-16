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
        try:
            t0 = time.monotonic()
            conn = http.client.HTTPSConnection("github.com", 443, context=ctx, timeout=300)
            conn.request(self.command, self.path, body=body, headers=headers)
            resp = conn.getresponse()
            latency_ms = (time.monotonic() - t0) * 1000
        except Exception as e:
            log(f"  UPSTREAM ERROR: {e}")
            self.send_error(502, str(e))
            return

        # Forward response
        resp_body = resp.read()
        conn.close()
        debug(f"<<< {resp.status} ({latency_ms:.0f}ms, {len(resp_body)} bytes)")
        self.send_response(resp.status)
        for key, value in resp.getheaders():
            if key.lower() in ("transfer-encoding", "connection", "keep-alive"):
                continue
            self.send_header(key, value)
        self.send_header("Content-Length", str(len(resp_body)))
        self.end_headers()
        self.wfile.write(resp_body)

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
