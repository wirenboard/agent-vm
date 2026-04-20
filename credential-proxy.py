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
import fcntl
import http.client
import http.server
import json
import os
import select
import signal
import socket
import socketserver
import ssl
import subprocess
import sys
import threading
import time
from urllib.parse import parse_qsl, urlparse, unquote

MAX_REQUEST_BODY = 32 * 1024 * 1024  # 32 MB
UPSTREAM_TIMEOUT = 300  # seconds
INBOUND_TIMEOUT = 60   # seconds
WEBSOCKET_IDLE_TIMEOUT = 300  # seconds

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


def _read_credentials_file(path):
    """Read and parse a credentials JSON file under an advisory shared lock.

    flock(LOCK_SH) already permits concurrent in-process readers; no extra
    threading.Lock needed. The shared lock blocks only while host Claude
    holds LOCK_EX to rewrite the file — i.e. exactly when we want readers
    to wait for the new tokens.
    """
    try:
        with open(path, "r") as f:
            fcntl.flock(f.fileno(), fcntl.LOCK_SH)
            try:
                return json.load(f)
            finally:
                fcntl.flock(f.fileno(), fcntl.LOCK_UN)
    except (OSError, json.JSONDecodeError) as e:
        debug(f"  credentials read failed: {path}: {e}")
        return None


def _json_path_get(data, dotted):
    cur = data
    for part in dotted.split("."):
        if not isinstance(cur, dict) or part not in cur:
            return None
        cur = cur[part]
    return cur


def _jwt_payload(token):
    """Decode a JWT payload (no signature verification) or return {}."""
    try:
        parts = (token or "").split(".")
        if len(parts) < 2:
            return {}
        pad = "=" * (-len(parts[1]) % 4)
        return json.loads(base64.urlsafe_b64decode(parts[1] + pad))
    except (ValueError, json.JSONDecodeError, UnicodeDecodeError):
        return {}


def _jwt_exp_seconds(token):
    exp = _jwt_payload(token).get("exp")
    try:
        return int(exp) if exp is not None else 0
    except (TypeError, ValueError):
        return 0


def _jwt_forge(payload):
    """Emit an unsigned JWT (header.payload.placeholder) — host Codex's own
    placeholder writer uses the same shape and Codex reads it back fine."""
    def b64(obj):
        return base64.urlsafe_b64encode(
            json.dumps(obj, separators=(",", ":")).encode()
        ).decode().rstrip("=")
    header = {"alg": "RS256", "typ": "JWT"}
    return f"{b64(header)}.{b64(payload)}.placeholder"


# Serializes refresh invocations per credentials file: we never want two
# refresh probes racing for the same file.
_refresh_locks = {}
_refresh_locks_guard = threading.Lock()


def _refresh_lock(path):
    with _refresh_locks_guard:
        lock = _refresh_locks.get(path)
        if lock is None:
            lock = threading.Lock()
            _refresh_locks[path] = lock
        return lock


_HOST_REFRESH_ENV_DROP = {
    "HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY",
    "https_proxy", "http_proxy", "no_proxy",
    "CLAUDE_CODE_OAUTH_TOKEN",
}


def _trigger_host_refresh(command, timeout):
    """Spawn a host agent (claude / codex) to force its credential refresh.

    The spawned process inherits the user env but with proxy vars stripped so
    it talks to the real upstream, not back to us.
    """
    env = {k: v for k, v in os.environ.items()
           if k not in _HOST_REFRESH_ENV_DROP}
    debug(f"  oauth_refresh: spawning {command}")
    try:
        result = subprocess.run(
            command, env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            timeout=timeout, check=False,
        )
        if result.returncode != 0:
            debug(f"  oauth_refresh: refresh command rc={result.returncode}: "
                  f"{result.stdout[:500]!r}")
            return False
        debug(f"  oauth_refresh: refresh command succeeded")
        return True
    except subprocess.TimeoutExpired:
        debug(f"  oauth_refresh: refresh command timed out after {timeout}s")
        return False
    except (FileNotFoundError, OSError) as e:
        debug(f"  oauth_refresh: refresh command failed to launch: {e}")
        return False


# Parse credential rules from env. See _handle_oauth_refresh() and
# _build_upstream_headers() for the supported rule fields (headers, path_prefix,
# use_proxy, auth_from_file, oauth_refresh).
_RULES = {}
_rules_json = os.environ.get("CREDENTIAL_PROXY_RULES", "[]")
try:
    for rule in json.loads(_rules_json):
        domain = rule["domain"]
        entry = {
            "headers": rule.get("headers", {}),
            "path_prefix": rule.get("path_prefix"),
            "use_proxy": rule.get("use_proxy", False),
            "auth_from_file": rule.get("auth_from_file"),
            "oauth_refresh": rule.get("oauth_refresh"),
            "oauth_refresh_codex": rule.get("oauth_refresh_codex"),
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


def _build_upstream_headers(request_headers, original_host, rule):
    """Copy inbound headers, remove proxy metadata, and apply injected headers."""
    headers = {}
    header_keys_lower = {}
    for key, value in request_headers.items():
        lower = key.lower()
        if lower in (
            "host", "accept-encoding",
            "x-original-host", "x-original-port", "x-original-scheme",
            "proxy-authorization",
        ):
            continue
        if lower in header_keys_lower:
            actual_key = header_keys_lower[lower]
            headers[actual_key] = f"{headers[actual_key]},{value}"
        else:
            headers[key] = value
            header_keys_lower[lower] = key

    headers["Host"] = original_host

    def _set_header(name, value):
        to_remove = []
        for existing_key in headers:
            if existing_key.lower() == name.lower():
                to_remove.append(existing_key)
        for key in to_remove:
            del headers[key]
            header_keys_lower.pop(key.lower(), None)
        headers[name] = value
        header_keys_lower[name.lower()] = name

    if rule:
        for header_name, header_value in rule["headers"].items():
            _set_header(header_name, header_value)
            debug(f"  injected {header_name}: {redact(header_value)}")

        auth_src = rule.get("auth_from_file")
        if auth_src:
            creds = _read_credentials_file(auth_src["path"])
            token = _json_path_get(creds or {}, auth_src["json_path"])
            if token:
                header_name = auth_src["header"]
                header_value = auth_src["format"].replace("{token}", token)
                _set_header(header_name, header_value)
                debug(f"  injected {header_name} from {auth_src['path']}: {redact(header_value)}")
            else:
                debug(f"  auth_from_file missing token at {auth_src['json_path']} in {auth_src['path']}")

    if DEBUG:
        for key, value in headers.items():
            lower = key.lower()
            if lower in ("authorization", "x-api-key"):
                debug(f"  > {key}: {redact(value)}")
            else:
                debug(f"  > {key}: {value}")

    return headers


def _open_upstream_connection(original_host, original_port, original_scheme, use_proxy):
    """Open an upstream HTTP(S) connection for non-upgrade requests."""
    if original_scheme == "https" and use_proxy:
        conn = http.client.HTTPSConnection(
            _ai_proxy["host"], _ai_proxy["port"], context=_ai_ssl_ctx, timeout=UPSTREAM_TIMEOUT
        )
        tunnel_headers = {}
        if "auth" in _ai_proxy:
            tunnel_headers["Proxy-Authorization"] = _ai_proxy["auth"]
        conn.set_tunnel(original_host, original_port, headers=tunnel_headers)
        return conn
    if original_scheme == "https":
        return http.client.HTTPSConnection(
            original_host, original_port, context=_ssl_ctx, timeout=UPSTREAM_TIMEOUT
        )
    return http.client.HTTPConnection(original_host, original_port, timeout=UPSTREAM_TIMEOUT)


def _open_upstream_socket(original_host, original_port, original_scheme, use_proxy):
    """Open a raw socket suitable for WebSocket upgrade forwarding."""
    if use_proxy:
        sock = socket.create_connection((_ai_proxy["host"], _ai_proxy["port"]), timeout=UPSTREAM_TIMEOUT)
        sock.settimeout(UPSTREAM_TIMEOUT)
        connect_lines = [f"CONNECT {original_host}:{original_port} HTTP/1.1"]
        connect_lines.append(f"Host: {original_host}:{original_port}")
        if "auth" in _ai_proxy:
            connect_lines.append(f"Proxy-Authorization: {_ai_proxy['auth']}")
        connect_lines.append("")
        connect_lines.append("")
        sock.sendall("\r\n".join(connect_lines).encode())
        response = b""
        while b"\r\n\r\n" not in response:
            chunk = sock.recv(4096)
            if not chunk:
                raise OSError("proxy closed CONNECT response")
            response += chunk
            if len(response) > 65536:
                raise OSError("CONNECT response too large")
        header_block, remainder = response.split(b"\r\n\r\n", 1)
        status_line = header_block.split(b"\r\n", 1)[0].decode("iso-8859-1", errors="replace")
        try:
            status_code = int(status_line.split(" ", 2)[1])
        except (IndexError, ValueError) as exc:
            raise OSError(f"invalid CONNECT response: {status_line}") from exc
        if status_code != 200:
            raise OSError(f"CONNECT failed: {status_line}")
        if remainder:
            raise OSError("unexpected buffered data after CONNECT")
        if original_scheme == "https":
            server_hostname = original_host
            ctx = _ai_ssl_ctx
            sock = ctx.wrap_socket(sock, server_hostname=server_hostname)
        return sock

    sock = socket.create_connection((original_host, original_port), timeout=UPSTREAM_TIMEOUT)
    sock.settimeout(UPSTREAM_TIMEOUT)
    if original_scheme == "https":
        sock = _ssl_ctx.wrap_socket(sock, server_hostname=original_host)
    return sock


def _read_http_response(sock):
    """Read an HTTP response head and return status/header info plus buffered body bytes."""
    response = b""
    while b"\r\n\r\n" not in response:
        chunk = sock.recv(4096)
        if not chunk:
            raise OSError("upstream closed before sending response headers")
        response += chunk
        if len(response) > 65536:
            raise OSError("response headers too large")
    header_block, remainder = response.split(b"\r\n\r\n", 1)
    lines = header_block.decode("iso-8859-1").split("\r\n")
    status_line = lines[0]
    parts = status_line.split(" ", 2)
    if len(parts) < 2:
        raise OSError(f"invalid upstream status line: {status_line}")
    status_code = int(parts[1])
    reason = parts[2] if len(parts) > 2 else ""
    headers = []
    for line in lines[1:]:
        if not line or ":" not in line:
            continue
        key, value = line.split(":", 1)
        headers.append((key.strip(), value.lstrip()))
    return status_code, reason, headers, remainder


def _tunnel_bidirectional(client_sock, upstream_sock, initial_upstream_data=b""):
    sockets = [client_sock, upstream_sock]
    if initial_upstream_data:
        client_sock.sendall(initial_upstream_data)
    while sockets:
        readable, _, exceptional = select.select(sockets, [], sockets, WEBSOCKET_IDLE_TIMEOUT)
        if exceptional:
            break
        if not readable:
            debug("  websocket tunnel idle timeout")
            break
        for current in readable:
            peer = upstream_sock if current is client_sock else client_sock
            try:
                data = current.recv(65536)
            except (socket.timeout, ssl.SSLWantReadError):
                continue
            if not data:
                return
            peer.sendall(data)


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

        # Apply credential rules for this domain (+ optional path prefix)
        rule = _match_rule(original_host, self.path)
        if not rule:
            debug(f"  no credential rule for {original_host}, forwarding as-is")

        if rule and rule.get("oauth_refresh"):
            self._handle_oauth_refresh(rule["oauth_refresh"], body)
            return
        if rule and rule.get("oauth_refresh_codex"):
            self._handle_codex_oauth_refresh(rule["oauth_refresh_codex"], body)
            return

        headers = _build_upstream_headers(self.headers, original_host, rule)

        # Connect to upstream (route through AI proxy if rule says so)
        upstream_port = original_port
        use_proxy = rule and rule.get("use_proxy") and _ai_proxy
        is_websocket = (
            self.headers.get("Upgrade", "").lower() == "websocket"
            and "upgrade" in self.headers.get("Connection", "").lower()
        )
        if is_websocket:
            try:
                t0 = time.monotonic()
                upstream_sock = _open_upstream_socket(
                    original_host, upstream_port, original_scheme, use_proxy
                )
                request_lines = [f"{self.command} {self.path} HTTP/1.1"]
                for key, value in headers.items():
                    request_lines.append(f"{key}: {value}")
                request_lines.append("")
                request_lines.append("")
                upstream_sock.sendall("\r\n".join(request_lines).encode())
                status_code, reason, upstream_headers, buffered = _read_http_response(upstream_sock)
                self.send_response(status_code, reason)
                for key, value in upstream_headers:
                    lower = key.lower()
                    if lower in ("transfer-encoding", "content-length", "keep-alive"):
                        continue
                    self.send_header(key, value)
                if status_code != 101:
                    self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.flush()
                latency_ms = (time.monotonic() - t0) * 1000
                debug(f"<<< {status_code} websocket-upgrade ({latency_ms:.0f}ms)")
                if status_code == 101:
                    self.connection.settimeout(WEBSOCKET_IDLE_TIMEOUT)
                    upstream_sock.settimeout(WEBSOCKET_IDLE_TIMEOUT)
                    try:
                        _tunnel_bidirectional(self.connection, upstream_sock, buffered)
                    finally:
                        upstream_sock.close()
                else:
                    if buffered:
                        self.wfile.write(buffered)
                        self.wfile.flush()
                    upstream_sock.close()
                self.close_connection = True
                return
            except Exception as e:
                debug(f"  WEBSOCKET UPSTREAM ERROR: {e}")
                self.send_error_response(502, f"Failed to connect to {original_host}")
                return
        try:
            t0 = time.monotonic()
            conn = _open_upstream_connection(original_host, upstream_port, original_scheme, use_proxy)
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

    def _handle_oauth_refresh(self, cfg, body):
        """Synthesize an OAuth refresh response by consulting the host file.

        If the host's access token is still valid, return it (as placeholders
        plus the real remaining lifetime). Otherwise, spawn host Claude to
        perform the actual OAuth refresh (host Claude writes the file), then
        re-read and return the fresh window — again as placeholders.
        """
        creds_path = cfg["credentials_file"]
        json_path = cfg.get("json_path", "claudeAiOauth")
        expiry_grace_ms = int(cfg.get("expiry_grace_ms", 60_000))
        placeholder_access = cfg.get(
            "placeholder_access_token", "sk-ant-oat01-placeholder-proxy-managed")
        placeholder_refresh = cfg.get(
            "placeholder_refresh_token", "sk-ant-ort01-placeholder-proxy-managed")
        refresh_command = cfg.get(
            "refresh_command", ["claude", "-p", "say hi and exit", "--model", "sonnet"])
        refresh_timeout = int(cfg.get("refresh_timeout", 120))

        try:
            body_json = json.loads(body) if body else {}
        except (json.JSONDecodeError, UnicodeDecodeError):
            body_json = {}
        if body_json.get("grant_type") not in (None, "refresh_token"):
            debug(f"  oauth_refresh: unsupported grant_type {body_json.get('grant_type')!r}")
            self.send_error_response(400, "unsupported grant_type")
            return

        def _current_oauth():
            creds = _read_credentials_file(creds_path) or {}
            return _json_path_get(creds, json_path) or {}

        def _expiry_ms(oauth):
            try:
                return int(oauth.get("expiresAt") or 0)
            except (TypeError, ValueError):
                return 0

        oauth = _current_oauth()
        if _expiry_ms(oauth) <= int(time.time() * 1000) + expiry_grace_ms:
            with _refresh_lock(creds_path):
                # Re-check: another thread may have refreshed while we waited.
                oauth = _current_oauth()
                if _expiry_ms(oauth) <= int(time.time() * 1000) + expiry_grace_ms:
                    ok = _trigger_host_refresh(refresh_command, refresh_timeout)
                    oauth = _current_oauth()
                    if not ok or _expiry_ms(oauth) <= int(time.time() * 1000):
                        debug("  oauth_refresh: host refresh did not update credentials")
                        self.send_error_response(502, "host refresh failed")
                        return

        expires_in = max(1, (_expiry_ms(oauth) - int(time.time() * 1000)) // 1000)
        scopes = oauth.get("scopes") or []
        response_body = {
            "token_type": "Bearer",
            "access_token": placeholder_access,
            "refresh_token": placeholder_refresh,
            "expires_in": int(expires_in),
        }
        if scopes:
            response_body["scope"] = " ".join(scopes)
        encoded = json.dumps(response_body).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(encoded)
        debug(f"  oauth_refresh: returned placeholder tokens, expires_in={expires_in}s")

    def _handle_codex_oauth_refresh(self, cfg, body):
        """MITM for auth.openai.com/oauth/token.

        The request body is application/x-www-form-urlencoded (Codex convention,
        not JSON). We read JWT `exp` from host's tokens.access_token; if stale,
        spawn host `codex exec` which does the real rotation and rewrites the
        file. Response is OAuth-shaped JSON with forged placeholder JWTs whose
        claims mirror the host's (account_id, plan_type, email, exp) so the VM
        Codex writes a valid-looking auth.json — real tokens stay on the host.
        """
        creds_path = cfg["credentials_file"]
        expiry_grace_ms = int(cfg.get("expiry_grace_ms", 60_000))
        refresh_command = cfg.get(
            "refresh_command", ["codex", "exec",
                                "--skip-git-repo-check",
                                "--dangerously-bypass-approvals-and-sandbox",
                                "--color", "never",
                                "Reply with exactly OK and nothing else."])
        refresh_timeout = int(cfg.get("refresh_timeout", 120))
        client_id = cfg.get("audience", "app_EMoamEEZ73f0CkXaXp7hrann")
        placeholder_refresh = cfg.get(
            "placeholder_refresh_token",
            "placeholder-refresh-token-injected-by-proxy")

        try:
            params = dict(parse_qsl((body or b"").decode("utf-8"), keep_blank_values=True))
        except UnicodeDecodeError:
            params = {}
        if params.get("grant_type") not in (None, "refresh_token"):
            debug(f"  oauth_refresh_codex: unsupported grant_type {params.get('grant_type')!r}")
            self.send_error_response(400, "unsupported grant_type")
            return

        def _host_access_jwt():
            creds = _read_credentials_file(creds_path) or {}
            return _json_path_get(creds, "tokens.access_token") or ""

        now_ms = lambda: int(time.time() * 1000)
        if _jwt_exp_seconds(_host_access_jwt()) * 1000 <= now_ms() + expiry_grace_ms:
            with _refresh_lock(creds_path):
                if _jwt_exp_seconds(_host_access_jwt()) * 1000 <= now_ms() + expiry_grace_ms:
                    ok = _trigger_host_refresh(refresh_command, refresh_timeout)
                    if not ok or _jwt_exp_seconds(_host_access_jwt()) * 1000 <= now_ms():
                        debug("  oauth_refresh_codex: host refresh did not update credentials")
                        self.send_error_response(502, "host refresh failed")
                        return

        creds = _read_credentials_file(creds_path) or {}
        tokens = creds.get("tokens") or {}
        access_payload = _jwt_payload(tokens.get("access_token"))
        id_payload = _jwt_payload(tokens.get("id_token"))
        auth_claims = (access_payload.get("https://api.openai.com/auth")
                       or id_payload.get("https://api.openai.com/auth") or {})
        account_id = (tokens.get("account_id")
                      or auth_claims.get("chatgpt_account_id"))
        plan_type = auth_claims.get("chatgpt_plan_type")
        email = (id_payload.get("email")
                 or (id_payload.get("https://api.openai.com/profile") or {}).get("email"))
        exp = int(access_payload.get("exp") or 0)
        iat = max(0, exp - 3600)
        shared_auth = {"chatgpt_account_id": account_id, "chatgpt_plan_type": plan_type}

        vm_id_payload = {
            "iss": "https://auth.openai.com",
            "aud": [client_id],
            "iat": iat, "exp": exp,
            "https://api.openai.com/auth": shared_auth,
        }
        if email:
            vm_id_payload["email"] = email
        vm_access_payload = {
            "iss": "https://auth.openai.com",
            "aud": ["https://api.openai.com/v1"],
            "iat": iat, "nbf": iat, "exp": exp,
            "scp": access_payload.get("scp") or [
                "openid", "profile", "email", "offline_access"],
            "https://api.openai.com/auth": shared_auth,
        }

        expires_in = max(1, exp - int(time.time()))
        response_body = {
            "token_type": "Bearer",
            "access_token": _jwt_forge(vm_access_payload),
            "id_token": _jwt_forge(vm_id_payload),
            "refresh_token": placeholder_refresh,
            "expires_in": expires_in,
        }
        encoded = json.dumps(response_body).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(encoded)
        debug(f"  oauth_refresh_codex: returned forged JWTs, expires_in={expires_in}s")

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
