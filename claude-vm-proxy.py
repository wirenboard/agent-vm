#!/usr/bin/env python3
"""
Host-side API proxy for Claude Code VMs.

Reads OAuth credentials from the host's ~/.claude/.credentials.json (or
ANTHROPIC_API_KEY env var) and injects them into requests forwarded to
api.anthropic.com. The VM never sees real credentials.

Usage:
    python3 claude-vm-proxy.py
    # Prints the listening port to stdout, then serves until SIGTERM.
"""

import fcntl
import http.client
import http.server
import json
import os
import signal
import ssl
import sys
import socketserver
import time
import threading

API_HOST = "api.anthropic.com"
API_PORT = 443
CREDENTIALS_PATH = os.path.expanduser("~/.claude/.credentials.json")
CREDENTIALS_DIR = os.path.expanduser("~/.claude")
TOKEN_HOST = "platform.claude.com"
TOKEN_PATH = "/v1/oauth/token"
_token_use_tls = True  # Tests set this to False to use plain HTTP mock server
OAUTH_CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
OAUTH_SCOPES = "user:profile user:inference user:sessions:claude_code user:mcp_servers"
EXPIRY_BUFFER_SECS = 300  # Refresh 5 minutes before expiry (matches Claude CLI)
DEBUG = os.environ.get("CLAUDE_VM_PROXY_DEBUG", "0") == "1"
_log_file = None
_refresh_lock = threading.Lock()


def _get_log_file():
    global _log_file
    if _log_file is None:
        log_path = os.path.join(os.environ.get("CLAUDE_VM_PROXY_LOG_DIR", "."), "claude-vm-proxy.log")
        _log_file = open(log_path, "a")
    return _log_file


def debug(msg):
    if DEBUG:
        line = f"[proxy {time.strftime('%H:%M:%S')}] {msg}\n"
        f = _get_log_file()
        f.write(line)
        f.flush()


def redact(value, show=8):
    """Show first `show` chars of a sensitive value."""
    if not value:
        return "<empty>"
    if len(value) <= show:
        return value
    return value[:show] + "..."


def _read_oauth_creds():
    """Read OAuth credentials from disk. Returns (creds_dict, oauth_section, error)."""
    try:
        with open(CREDENTIALS_PATH, "r") as f:
            creds = json.load(f)
    except FileNotFoundError:
        return None, None, (
            "No credentials found. Set ANTHROPIC_API_KEY or run 'claude' on "
            "the host to create ~/.claude/.credentials.json"
        )
    except (json.JSONDecodeError, OSError) as e:
        return None, None, f"Failed to read credentials: {e}"

    oauth = creds.get("claudeAiOauth", {})
    return creds, oauth, None


def _is_token_expiring(oauth):
    """Check if token is expired or within EXPIRY_BUFFER_SECS of expiry."""
    expires_at = oauth.get("expiresAt")
    if not expires_at:
        return False
    try:
        expires_ts = float(expires_at) / 1000
    except (ValueError, TypeError):
        return False
    return time.time() + EXPIRY_BUFFER_SECS > expires_ts


def _refresh_oauth_token(refresh_token):
    """Exchange refresh_token for a new access_token via Anthropic's OAuth endpoint.
    Returns (new_oauth_dict, error_message)."""
    payload = json.dumps({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": OAUTH_CLIENT_ID,
        "scope": OAUTH_SCOPES,
    }).encode()

    try:
        if _token_use_tls:
            conn = http.client.HTTPSConnection(TOKEN_HOST, timeout=30)
        else:
            conn = http.client.HTTPConnection(TOKEN_HOST, timeout=30)
        try:
            conn.request("POST", TOKEN_PATH, body=payload, headers={
                "Content-Type": "application/json",
            })
            resp = conn.getresponse()
            body = resp.read()
        finally:
            conn.close()
        if resp.status != 200:
            debug(f"Token refresh HTTP {resp.status}: {body[:200].decode(errors='replace')}")
            return None, f"Token refresh failed (HTTP {resp.status})"
        data = json.loads(body)
    except Exception as e:
        return None, f"Token refresh failed: {e}"

    access_token = data.get("access_token")
    if not access_token:
        return None, "Token refresh response missing access_token"

    new_refresh = data.get("refresh_token", refresh_token)
    expires_in = int(data.get("expires_in", 3600))
    expires_at = int(time.time() * 1000) + expires_in * 1000
    scopes = data.get("scope", OAUTH_SCOPES).split()

    new_oauth = {
        "accessToken": access_token,
        "refreshToken": new_refresh,
        "expiresAt": expires_at,
        "scopes": scopes,
    }
    return new_oauth, None


def _save_credentials(creds):
    """Write credentials back to disk atomically with a file lock."""
    lock_path = os.path.join(CREDENTIALS_DIR, ".credentials.lock")
    tmp_path = CREDENTIALS_PATH + ".tmp"
    try:
        fd = os.open(lock_path, os.O_CREAT | os.O_RDWR, 0o600)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX)
            # Re-read to merge with any concurrent changes to other keys
            try:
                with open(CREDENTIALS_PATH, "r") as f:
                    disk_creds = json.load(f)
            except (FileNotFoundError, json.JSONDecodeError):
                disk_creds = {}
            disk_creds["claudeAiOauth"] = creds["claudeAiOauth"]
            with open(tmp_path, "w") as f:
                json.dump(disk_creds, f, indent=2)
            os.chmod(tmp_path, 0o600)
            os.replace(tmp_path, CREDENTIALS_PATH)
        finally:
            fcntl.flock(fd, fcntl.LOCK_UN)
            os.close(fd)
    except OSError as e:
        print(f"[proxy] WARNING: Failed to save credentials: {e}", file=sys.stderr)
        debug(f"Failed to save credentials: {e}")
        # Clean up tmp file if it exists
        try:
            os.unlink(tmp_path)
        except OSError:
            pass


def _ensure_fresh_token():
    """Ensure we have a valid, non-expired OAuth token.
    Re-reads credentials (host Claude may have refreshed), and if still
    expired, performs the refresh and saves the result.
    Returns (token, error_message)."""

    # Re-read from disk (host Claude CLI may have refreshed already)
    creds, oauth, err = _read_oauth_creds()
    if err:
        return None, err

    token = oauth.get("accessToken")
    if not token:
        return None, (
            "No accessToken in ~/.claude/.credentials.json. "
            "Run 'claude' on the host to authenticate."
        )

    if not _is_token_expiring(oauth):
        return token, None

    # Token is expiring/expired — need to refresh
    refresh_token = oauth.get("refreshToken")
    if not refresh_token:
        return None, (
            "OAuth token expired and no refreshToken available. "
            "Run 'claude' on the host to re-authenticate."
        )

    debug("OAuth token expiring, attempting refresh...")
    new_oauth, refresh_err = _refresh_oauth_token(refresh_token)
    if refresh_err:
        debug(f"Token refresh failed: {refresh_err}")
        return None, f"OAuth token expired, refresh failed: {refresh_err}"

    # Preserve subscription info from the old credentials
    for key in ("subscriptionType", "rateLimitTier"):
        if key in oauth and key not in new_oauth:
            new_oauth[key] = oauth[key]

    creds["claudeAiOauth"] = new_oauth
    _save_credentials(creds)
    debug(f"Token refreshed, new expiry: {new_oauth['expiresAt']}")
    return new_oauth["accessToken"], None


def get_auth_token():
    """Return (token, is_oauth, error_message). error_message is None on success."""
    # Priority 1: explicit API key
    api_key = os.environ.get("ANTHROPIC_API_KEY")
    if api_key:
        return api_key, False, None

    # Priority 2: OAuth credentials file (with auto-refresh)
    # Fast path: if token is valid, return without acquiring the lock
    creds, oauth, read_err = _read_oauth_creds()
    if not read_err and oauth.get("accessToken") and not _is_token_expiring(oauth):
        return oauth["accessToken"], True, None
    # Slow path: lock and refresh (re-checks inside to handle races)
    with _refresh_lock:
        token, err = _ensure_fresh_token()
    if err:
        return None, False, err
    return token, True, None


class ProxyHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    # Suppress default request logging
    def log_message(self, format, *args):
        pass

    def do_request(self):
        token, is_oauth, err = get_auth_token()
        if err:
            debug(f"AUTH ERROR: {err}")
            # Don't leak detailed error messages (may contain credential fragments)
            self.send_error_response(401, "Authentication failed. Check proxy logs.")
            return

        # Read request body
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length) if content_length else None

        debug(f">>> {self.command} {self.path} ({content_length} bytes)")
        if DEBUG:
            for key, value in self.headers.items():
                lower = key.lower()
                if lower in ("x-api-key", "authorization"):
                    debug(f"  > {key}: {redact(value)}")
                else:
                    debug(f"  > {key}: {value}")
            if body:
                try:
                    body_json = json.loads(body)
                    model = body_json.get("model", "?")
                    stream = body_json.get("stream", False)
                    n_msgs = len(body_json.get("messages", []))
                    debug(f"  > body: model={model} stream={stream} messages={n_msgs}")
                except (json.JSONDecodeError, AttributeError):
                    debug(f"  > body: {len(body)} bytes (not JSON)")

        # Build upstream headers: copy all, replace auth
        # Use case-insensitive tracking to handle duplicate keys correctly
        headers = {}
        header_keys_lower = {}  # lower -> actual key used in headers
        for key, value in self.headers.items():
            lower = key.lower()
            if lower in ("x-api-key", "authorization", "host"):
                continue
            if lower in header_keys_lower:
                # Append duplicate header values (e.g. multiple anthropic-beta)
                actual_key = header_keys_lower[lower]
                headers[actual_key] = f"{headers[actual_key]},{value}"
            else:
                headers[key] = value
                header_keys_lower[lower] = key

        if is_oauth:
            # OAuth tokens use Bearer auth and require the oauth beta header
            headers["Authorization"] = f"Bearer {token}"
            # Merge oauth beta with any existing anthropic-beta values
            oauth_beta = "oauth-2025-04-20"
            beta_key = header_keys_lower.get("anthropic-beta")
            if beta_key:
                if oauth_beta not in headers[beta_key]:
                    headers[beta_key] = f"{headers[beta_key]},{oauth_beta}"
            else:
                headers["anthropic-beta"] = oauth_beta
            debug(f"  auth: OAuth Bearer {redact(token)}")
        else:
            # API keys use x-api-key header
            headers["x-api-key"] = token
            debug(f"  auth: API key {redact(token)}")

        headers["Host"] = API_HOST

        if DEBUG:
            debug(f"  -> {API_HOST}:{API_PORT} {self.command} {self.path}")
            for key, value in headers.items():
                lower = key.lower()
                if lower in ("x-api-key", "authorization"):
                    debug(f"    {key}: {redact(value)}")
                else:
                    debug(f"    {key}: {value}")

        # Connect to upstream
        ctx = ssl.create_default_context()
        try:
            t0 = time.monotonic()
            conn = http.client.HTTPSConnection(API_HOST, API_PORT, context=ctx, timeout=300)
            conn.request(self.command, self.path, body=body, headers=headers)
            upstream = conn.getresponse()
            latency_ms = (time.monotonic() - t0) * 1000
        except Exception as e:
            debug(f"  UPSTREAM ERROR: {e}")
            self.send_error_response(502, f"Failed to connect to API: {e}")
            return

        # Determine if upstream used chunked transfer (i.e. streaming)
        # Python's http.client auto-decodes chunked, so we must re-frame.
        is_streaming = False
        upstream_headers = upstream.getheaders()
        for key, value in upstream_headers:
            if key.lower() == "transfer-encoding":
                is_streaming = True
                break

        debug(f"<<< {upstream.status} {'streaming' if is_streaming else 'complete'} ({latency_ms:.0f}ms)")
        if DEBUG:
            for key, value in upstream_headers:
                debug(f"  < {key}: {value}")

        if is_streaming:
            # Streaming: forward headers, re-chunk the decoded body
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

    def send_error_response(self, code, message):
        body = json.dumps({"error": {"type": "proxy_error", "message": message}}).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    # Handle all HTTP methods
    do_GET = do_request
    do_POST = do_request
    do_PUT = do_request
    do_PATCH = do_request
    do_DELETE = do_request
    do_HEAD = do_request
    do_OPTIONS = do_request


class QuietServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    allow_reuse_address = True
    daemon_threads = True


def main():
    # Verify credentials are available before starting
    _, _, err = get_auth_token()
    if err:
        print(f"Error: {err}", file=sys.stderr)
        sys.exit(1)

    server = QuietServer(("127.0.0.1", 0), ProxyHandler)
    port = server.server_address[1]

    # Print port for parent process to capture, then flush
    print(port, flush=True)

    # Graceful shutdown on SIGTERM
    def handle_signal(signum, frame):
        server.shutdown()

    signal.signal(signal.SIGTERM, handle_signal)
    signal.signal(signal.SIGINT, handle_signal)

    server.serve_forever()


if __name__ == "__main__":
    main()
