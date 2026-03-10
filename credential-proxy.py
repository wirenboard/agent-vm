#!/usr/bin/env python3
"""
Host-side unified credential proxy for agent VMs.

Receives HTTP requests from the VM's mitmproxy with X-Original-Host header,
matches against credential rules, injects auth headers, and forwards to
the real upstream over HTTPS.

The VM never sees real credentials.

Usage:
    CREDENTIAL_PROXY_RULES='[{"domain":"api.anthropic.com","headers":{"Authorization":"Bearer sk-..."}}]' \
        python3 credential-proxy.py
    # Prints the listening port to stdout, then serves until SIGTERM.

Optional env vars:
    CREDENTIAL_PROXY_SECRET    Shared secret; if set, requests must include
                               Proxy-Authorization header (407 otherwise)
    CREDENTIAL_PROXY_DEBUG     Set to "1" for verbose logging
    CREDENTIAL_PROXY_LOG_DIR   Directory for log file (default: current dir)
    AI_HTTPS_PROXY             Upstream proxy for AI API connections only
                               (e.g. http://user:pass@localhost:8082)
                               Only used for rules with "use_proxy": true
    AI_SSL_CERT_FILE           Path to additional CA certificate PEM file
                               (for AI_HTTPS_PROXY's TLS)
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
import threading
import time
from urllib.parse import urlparse, unquote

MAX_REQUEST_BODY = 32 * 1024 * 1024  # 32 MB
UPSTREAM_TIMEOUT = 300  # seconds
INBOUND_TIMEOUT = 60   # seconds

PROXY_SECRET = os.environ.get("CREDENTIAL_PROXY_SECRET", "")
DEBUG = os.environ.get("CREDENTIAL_PROXY_DEBUG", "0") == "1"
LOG_DIR = os.environ.get("CREDENTIAL_PROXY_LOG_DIR", ".")
AI_SSL_CERT_FILE = os.environ.get("AI_SSL_CERT_FILE", "")
_log_file = None

# Upstream proxy for AI API connections (rules with "use_proxy": true)
_ai_proxy = None
_AI_HTTPS_PROXY = os.environ.get("AI_HTTPS_PROXY", "")
if _AI_HTTPS_PROXY:
    _p = urlparse(_AI_HTTPS_PROXY)
    _ai_proxy = {"host": _p.hostname, "port": _p.port or 8080}
    if _p.username:
        _creds = base64.b64encode(
            f"{unquote(_p.username)}:{unquote(_p.password or '')}".encode()
        ).decode()
        _ai_proxy["auth"] = f"Basic {_creds}"

# SSL contexts: one for AI proxy connections, one for direct connections
_ssl_ctx = ssl.create_default_context()
_ai_ssl_ctx = ssl.create_default_context()
if AI_SSL_CERT_FILE:
    _ai_ssl_ctx.load_verify_locations(AI_SSL_CERT_FILE)


def _get_log_file():
    global _log_file
    if _log_file is None:
        log_path = os.path.join(LOG_DIR, "credential-proxy.log")
        fd = os.open(log_path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        _log_file = os.fdopen(fd, "a")
    return _log_file


def debug(msg):
    if DEBUG:
        line = f"[credential-proxy {time.strftime('%H:%M:%S')}] {msg}\n"
        f = _get_log_file()
        f.write(line)
        f.flush()


def redact(value, show=8):
    if not value:
        return "<empty>"
    if len(value) <= show:
        return value
    return value[:show] + "..."


# Parse credential rules from env
# Each rule: {"domain": "...", "headers": {...}, "path_prefix": "/optional/prefix",
#             "use_proxy": true}
# Multiple rules per domain are supported; longest matching path_prefix wins.
# Rules without path_prefix match any path on that domain (fallback).
# Rules with "use_proxy": true route through AI_HTTPS_PROXY.
_RULES = {}  # domain -> [{"headers": {...}, "path_prefix": str|None, "use_proxy": bool}, ...]
_rules_json = os.environ.get("CREDENTIAL_PROXY_RULES", "[]")
try:
    for rule in json.loads(_rules_json):
        domain = rule["domain"]
        entry = {
            "headers": rule.get("headers", {}),
            "path_prefix": rule.get("path_prefix"),
            "use_proxy": rule.get("use_proxy", False),
        }
        _RULES.setdefault(domain, []).append(entry)
    # Sort each domain's rules: longest path_prefix first, None last
    for domain in _RULES:
        _RULES[domain].sort(
            key=lambda r: (r["path_prefix"] is None, -(len(r["path_prefix"] or ""))))
except (json.JSONDecodeError, KeyError, TypeError) as e:
    print(f"Error: invalid CREDENTIAL_PROXY_RULES: {e}", file=sys.stderr)
    sys.exit(1)


def _match_rule(domain, path):
    """Find the best matching rule for domain + path."""
    candidates = _RULES.get(domain)
    if not candidates:
        return None
    for rule in candidates:
        prefix = rule["path_prefix"]
        if prefix is None or path.startswith(prefix):
            return rule
    return None


class CredentialProxyHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):
        pass

    def do_request(self):
        # Verify shared secret via standard proxy auth (prevents cross-VM credential theft)
        if PROXY_SECRET:
            proxy_auth = self.headers.get("Proxy-Authorization", "")
            expected = "Basic " + base64.b64encode(f"_:{PROXY_SECRET}".encode()).decode()
            if proxy_auth != expected:
                debug(f"REJECTED: invalid Proxy-Authorization from {self.client_address[0]}")
                self.send_error_response(407, "Proxy authentication required")
                return

        # Extract original host from mitmproxy header
        original_host = self.headers.get("X-Original-Host", "")
        original_port = int(self.headers.get("X-Original-Port", "443"))
        original_scheme = self.headers.get("X-Original-Scheme", "https")

        if not original_host:
            self.send_error_response(400, "Missing X-Original-Host header")
            return

        # Read request body with size limit
        try:
            content_length = int(self.headers.get("Content-Length", 0))
        except (ValueError, TypeError):
            self.send_error_response(400, "Invalid Content-Length")
            return
        if content_length < 0 or content_length > MAX_REQUEST_BODY:
            self.send_error_response(413, "Request body too large")
            return
        body = self.rfile.read(content_length) if content_length else None

        debug(f">>> {self.command} {self.path} (host={original_host}, {content_length} bytes)")

        # Build upstream headers: copy all, strip proxy metadata and accept-encoding
        headers = {}
        header_keys_lower = {}
        for key, value in self.headers.items():
            lower = key.lower()
            if lower in ("host", "accept-encoding",
                         "x-original-host", "x-original-port", "x-original-scheme",
                         "proxy-authorization"):
                continue
            if lower in header_keys_lower:
                actual_key = header_keys_lower[lower]
                headers[actual_key] = f"{headers[actual_key]},{value}"
            else:
                headers[key] = value
                header_keys_lower[lower] = key

        headers["Host"] = original_host

        # Apply credential rules for this domain (+ optional path prefix)
        rule = _match_rule(original_host, self.path)
        if rule:
            for header_name, header_value in rule["headers"].items():
                # Remove existing header with same name (case-insensitive)
                to_remove = []
                for existing_key in headers:
                    if existing_key.lower() == header_name.lower():
                        to_remove.append(existing_key)
                for k in to_remove:
                    del headers[k]
                    lower = k.lower()
                    if lower in header_keys_lower:
                        del header_keys_lower[lower]
                headers[header_name] = header_value
                header_keys_lower[header_name.lower()] = header_name
                debug(f"  injected {header_name}: {redact(header_value)}")
        else:
            debug(f"  no credential rule for {original_host}, forwarding as-is")

        if DEBUG:
            for key, value in headers.items():
                lower = key.lower()
                if lower in ("authorization", "x-api-key"):
                    debug(f"  > {key}: {redact(value)}")
                else:
                    debug(f"  > {key}: {value}")

        # Connect to upstream (route through AI proxy if rule says so)
        upstream_port = original_port
        use_proxy = rule and rule.get("use_proxy") and _ai_proxy
        try:
            t0 = time.monotonic()
            if original_scheme == "https" and use_proxy:
                conn = http.client.HTTPSConnection(
                    _ai_proxy["host"], _ai_proxy["port"],
                    context=_ai_ssl_ctx, timeout=UPSTREAM_TIMEOUT)
                tunnel_headers = {}
                if "auth" in _ai_proxy:
                    tunnel_headers["Proxy-Authorization"] = _ai_proxy["auth"]
                conn.set_tunnel(original_host, upstream_port, headers=tunnel_headers)
            elif original_scheme == "https":
                conn = http.client.HTTPSConnection(
                    original_host, upstream_port, context=_ssl_ctx, timeout=UPSTREAM_TIMEOUT)
            else:
                conn = http.client.HTTPConnection(
                    original_host, upstream_port, timeout=UPSTREAM_TIMEOUT)
            conn.request(self.command, self.path, body=body, headers=headers)
            upstream = conn.getresponse()
            latency_ms = (time.monotonic() - t0) * 1000
        except Exception as e:
            debug(f"  UPSTREAM ERROR: {e}")
            self.send_error_response(502, f"Failed to connect to {original_host}")
            return

        # Detect streaming (chunked transfer)
        is_streaming = False
        upstream_headers = upstream.getheaders()
        for key, value in upstream_headers:
            if key.lower() == "transfer-encoding":
                is_streaming = True
                break

        debug(f"<<< {upstream.status} {'streaming' if is_streaming else 'complete'} ({latency_ms:.0f}ms)")

        if is_streaming:
            # Streaming: re-chunk the decoded body
            self.send_response(upstream.status)
            for key, value in upstream_headers:
                lower = key.lower()
                if lower in ("transfer-encoding", "content-length",
                             "connection", "keep-alive"):
                    continue
                self.send_header(key, value)
            self.send_header("Transfer-Encoding", "chunked")
            self.send_header("Connection", "close")
            self.close_connection = True
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
                elapsed_ms = (time.monotonic() - t0) * 1000
                debug(f"  streamed {total_bytes} bytes ({elapsed_ms:.0f}ms total)")
            except (BrokenPipeError, ConnectionResetError) as e:
                debug(f"  stream broken: {e} after {total_bytes} bytes")
            finally:
                conn.close()
        else:
            # Non-streaming: read full body, send with Content-Length
            body_data = upstream.read()
            conn.close()
            debug(f"  body: {len(body_data)} bytes")
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
    do_PATCH = do_request
    do_DELETE = do_request
    do_HEAD = do_request
    do_OPTIONS = do_request


class QuietServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    allow_reuse_address = False
    daemon_threads = True
    timeout = INBOUND_TIMEOUT


def main():
    if not _RULES:
        print("Error: CREDENTIAL_PROXY_RULES is empty or not set", file=sys.stderr)
        sys.exit(1)

    server = QuietServer(("127.0.0.1", 0), CredentialProxyHandler)
    port = server.server_address[1]

    print(port, flush=True)

    def handle_signal(signum, frame):
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, handle_signal)
    signal.signal(signal.SIGINT, handle_signal)

    rule_summary = []
    for domain, rules in _RULES.items():
        for r in rules:
            prefix = r["path_prefix"]
            rule_summary.append(f"{domain}{prefix or ''}")
    debug(f"Listening on port {port}")
    debug(f"Credential rules for: {', '.join(rule_summary)}")
    if _ai_proxy:
        debug(f"AI upstream proxy: {_ai_proxy['host']}:{_ai_proxy['port']}")
    if AI_SSL_CERT_FILE:
        debug(f"AI SSL cert: {AI_SSL_CERT_FILE}")
    server.serve_forever()


if __name__ == "__main__":
    main()
