#!/usr/bin/env python3
"""Tests for credential-proxy.py.

Starts a real credential proxy and a mock upstream HTTP server,
then verifies header injection, stripping, streaming, error handling, etc.
"""

import base64
import http.client
import http.server
import json
import os
import signal
import subprocess
import sys
import threading
import unittest


def _stop_proxy(proc):
    """Stop a credential proxy subprocess cleanly."""
    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=2)


def _start_mock_upstream(handler_class):
    """Start a mock HTTP server, return (server, port)."""
    server = http.server.HTTPServer(("127.0.0.1", 0), handler_class)
    server.daemon_threads = True
    t = threading.Thread(target=server.serve_forever, daemon=True)
    t.start()
    return server, server.server_address[1]


def _start_credential_proxy(rules, env_extra=None):
    """Start credential-proxy.py as a subprocess, return (proc, port)."""
    env = os.environ.copy()
    env["CREDENTIAL_PROXY_RULES"] = json.dumps(rules)
    env.pop("CREDENTIAL_PROXY_DEBUG", None)
    if env_extra:
        env.update(env_extra)
    proc = subprocess.Popen(
        [sys.executable, "credential-proxy.py"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    line = proc.stdout.readline().decode().strip()
    if not line.isdigit():
        proc.kill()
        stderr = proc.stderr.read().decode()
        raise RuntimeError(f"Proxy failed to start: {line!r} stderr={stderr!r}")
    return proc, int(line)


def _proxy_request(port, method, path, headers=None, body=None):
    """Send a request to the credential proxy, return (status, resp_headers, body)."""
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    hdrs = headers or {}
    conn.request(method, path, body=body, headers=hdrs)
    resp = conn.getresponse()
    data = resp.read()
    resp_headers = dict(resp.getheaders())
    conn.close()
    return resp.status, resp_headers, data


# ── Mock upstream that echoes request info ─────────────────────────────────


class EchoHandler(http.server.BaseHTTPRequestHandler):
    """Mock upstream: returns JSON with received method, path, headers, body."""

    def log_message(self, *a):
        pass

    def _handle(self):
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length) if content_length else b""
        resp = json.dumps({
            "method": self.command,
            "path": self.path,
            "headers": dict(self.headers.items()),
            "body": body.decode("utf-8", errors="replace"),
        }).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(resp)))
        self.end_headers()
        self.wfile.write(resp)

    do_GET = _handle
    do_POST = _handle
    do_PUT = _handle
    do_PATCH = _handle
    do_DELETE = _handle


class ChunkedHandler(http.server.BaseHTTPRequestHandler):
    """Mock upstream: returns a chunked response (3 chunks)."""

    def log_message(self, *a):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Transfer-Encoding", "chunked")
        self.send_header("Content-Type", "text/plain")
        self.end_headers()
        for i in range(3):
            chunk = f"chunk-{i}\n".encode()
            self.wfile.write(f"{len(chunk):x}\r\n".encode())
            self.wfile.write(chunk)
            self.wfile.write(b"\r\n")
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()


class ErrorHandler(http.server.BaseHTTPRequestHandler):
    """Mock upstream: returns 500."""

    def log_message(self, *a):
        pass

    def do_GET(self):
        self.send_response(500)
        body = b'{"error": "server error"}'
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


# ── Tests ──────────────────────────────────────────────────────────────────


class TestCredentialProxy(unittest.TestCase):
    """Tests that use a mock upstream (HTTP, not HTTPS) to verify proxy behavior."""

    @classmethod
    def setUpClass(cls):
        # Start mock upstream
        cls.upstream_server, cls.upstream_port = _start_mock_upstream(EchoHandler)

        # Start credential proxy with rules pointing to our mock upstream
        # We use X-Original-Scheme: http so the proxy connects via HTTP
        cls.rules = [
            {
                "domain": f"127.0.0.1",
                "headers": {
                    "Authorization": "Bearer injected-secret-token",
                    "X-Custom-Header": "custom-value",
                },
            },
        ]
        cls.proxy_proc, cls.proxy_port = _start_credential_proxy(cls.rules)

    @classmethod
    def tearDownClass(cls):
        _stop_proxy(cls.proxy_proc)
        cls.upstream_server.shutdown()

    def _request(self, method="GET", path="/test", extra_headers=None, body=None):
        headers = {
            "X-Original-Host": "127.0.0.1",
            "X-Original-Port": str(self.upstream_port),
            "X-Original-Scheme": "http",
        }
        if extra_headers:
            headers.update(extra_headers)
        return _proxy_request(self.proxy_port, method, path, headers, body)

    def test_header_injection(self):
        """Proxy injects configured auth headers."""
        status, _, data = self._request()
        self.assertEqual(status, 200)
        echo = json.loads(data)
        self.assertEqual(echo["headers"]["Authorization"], "Bearer injected-secret-token")
        self.assertEqual(echo["headers"]["X-Custom-Header"], "custom-value")

    def test_host_header_set(self):
        """Proxy sets Host header to the original host."""
        status, _, data = self._request()
        echo = json.loads(data)
        self.assertEqual(echo["headers"]["Host"], "127.0.0.1")

    def test_accept_encoding_stripped(self):
        """Proxy strips Accept-Encoding from upstream request."""
        status, _, data = self._request(extra_headers={"Accept-Encoding": "gzip, deflate"})
        echo = json.loads(data)
        # Python's http.client adds "identity" automatically, but "gzip" must be gone
        ae = echo["headers"].get("Accept-Encoding", "")
        self.assertNotIn("gzip", ae)
        self.assertNotIn("deflate", ae)

    def test_proxy_metadata_stripped(self):
        """X-Original-* headers are NOT forwarded to upstream."""
        status, _, data = self._request()
        echo = json.loads(data)
        for key in echo["headers"]:
            self.assertFalse(key.startswith("X-Original-"),
                             f"Proxy metadata header {key} leaked upstream")

    def test_existing_auth_overwritten(self):
        """Proxy overwrites existing Authorization header from client."""
        status, _, data = self._request(
            extra_headers={"Authorization": "Bearer old-token"}
        )
        echo = json.loads(data)
        self.assertEqual(echo["headers"]["Authorization"], "Bearer injected-secret-token")

    def test_post_with_body(self):
        """POST with body is forwarded correctly."""
        body = json.dumps({"model": "test", "stream": False}).encode()
        status, _, data = self._request(
            method="POST", path="/v1/messages",
            extra_headers={"Content-Type": "application/json"},
            body=body,
        )
        self.assertEqual(status, 200)
        echo = json.loads(data)
        self.assertEqual(echo["method"], "POST")
        self.assertEqual(echo["path"], "/v1/messages")
        self.assertEqual(json.loads(echo["body"])["model"], "test")

    def test_put_method(self):
        """PUT method works."""
        status, _, data = self._request(method="PUT", body=b"data")
        self.assertEqual(status, 200)
        echo = json.loads(data)
        self.assertEqual(echo["method"], "PUT")

    def test_delete_method(self):
        """DELETE method works."""
        status, _, data = self._request(method="DELETE")
        self.assertEqual(status, 200)
        echo = json.loads(data)
        self.assertEqual(echo["method"], "DELETE")

    def test_patch_method(self):
        """PATCH method works."""
        status, _, data = self._request(method="PATCH", body=b"patch-data")
        self.assertEqual(status, 200)
        echo = json.loads(data)
        self.assertEqual(echo["method"], "PATCH")

    def test_path_preserved(self):
        """Request path is forwarded unchanged."""
        status, _, data = self._request(path="/v1/messages?foo=bar")
        echo = json.loads(data)
        self.assertEqual(echo["path"], "/v1/messages?foo=bar")

    def test_duplicate_headers_merged(self):
        """Duplicate header values are merged with comma."""
        # Send two X-Test values
        conn = http.client.HTTPConnection("127.0.0.1", self.proxy_port, timeout=10)
        conn.putrequest("GET", "/test")
        conn.putheader("X-Original-Host", "127.0.0.1")
        conn.putheader("X-Original-Port", str(self.upstream_port))
        conn.putheader("X-Original-Scheme", "http")
        conn.putheader("X-Test", "value1")
        conn.putheader("X-Test", "value2")
        conn.endheaders()
        resp = conn.getresponse()
        data = resp.read()
        conn.close()
        echo = json.loads(data)
        self.assertIn("value1", echo["headers"].get("X-Test", ""))
        self.assertIn("value2", echo["headers"].get("X-Test", ""))


class TestCredentialProxyErrors(unittest.TestCase):
    """Error handling tests."""

    @classmethod
    def setUpClass(cls):
        cls.rules = [{"domain": "test.example", "headers": {}}]
        cls.proxy_proc, cls.proxy_port = _start_credential_proxy(cls.rules)

    @classmethod
    def tearDownClass(cls):
        _stop_proxy(cls.proxy_proc)

    def test_missing_x_original_host(self):
        """Returns 400 when X-Original-Host is missing."""
        status, _, data = _proxy_request(self.proxy_port, "GET", "/test")
        self.assertEqual(status, 400)
        body = json.loads(data)
        self.assertIn("Missing X-Original-Host", body["error"]["message"])

    def test_body_too_large(self):
        """Returns 413 when Content-Length exceeds limit."""
        headers = {
            "X-Original-Host": "test.example",
            "Content-Length": str(64 * 1024 * 1024),  # 64 MB > 32 MB limit
        }
        status, _, data = _proxy_request(self.proxy_port, "POST", "/test", headers)
        self.assertEqual(status, 413)

    def test_invalid_content_length(self):
        """Returns 400 when Content-Length is not a number."""
        headers = {
            "X-Original-Host": "test.example",
            "Content-Length": "not-a-number",
        }
        status, _, data = _proxy_request(self.proxy_port, "POST", "/test", headers)
        self.assertEqual(status, 400)

    def test_upstream_unreachable(self):
        """Returns 502 when upstream is unreachable."""
        headers = {
            "X-Original-Host": "127.0.0.1",
            "X-Original-Port": "1",  # nothing listening
            "X-Original-Scheme": "http",
        }
        status, _, data = _proxy_request(self.proxy_port, "GET", "/test", headers)
        self.assertEqual(status, 502)


class TestCredentialProxySecret(unittest.TestCase):
    """Tests for shared secret verification (cross-VM isolation)."""

    @classmethod
    def setUpClass(cls):
        cls.upstream_server, cls.upstream_port = _start_mock_upstream(EchoHandler)
        cls.secret = "test-secret-token-12345"
        cls.rules = [
            {"domain": "127.0.0.1", "headers": {"Authorization": "Bearer secret-value"}},
        ]
        cls.proxy_proc, cls.proxy_port = _start_credential_proxy(
            cls.rules, env_extra={"CREDENTIAL_PROXY_SECRET": cls.secret})

    @classmethod
    def tearDownClass(cls):
        _stop_proxy(cls.proxy_proc)
        cls.upstream_server.shutdown()

    def test_valid_secret_accepted(self):
        """Request with correct X-Proxy-Token succeeds."""
        headers = {
            "X-Original-Host": "127.0.0.1",
            "X-Original-Port": str(self.upstream_port),
            "X-Original-Scheme": "http",
            "X-Proxy-Token": self.secret,
        }
        status, _, data = _proxy_request(self.proxy_port, "GET", "/test", headers)
        self.assertEqual(status, 200)
        echo = json.loads(data)
        self.assertEqual(echo["headers"]["Authorization"], "Bearer secret-value")

    def test_missing_secret_rejected(self):
        """Request without X-Proxy-Token returns 403."""
        headers = {
            "X-Original-Host": "127.0.0.1",
            "X-Original-Port": str(self.upstream_port),
            "X-Original-Scheme": "http",
        }
        status, _, data = _proxy_request(self.proxy_port, "GET", "/test", headers)
        self.assertEqual(status, 403)

    def test_wrong_secret_rejected(self):
        """Request with wrong X-Proxy-Token returns 403."""
        headers = {
            "X-Original-Host": "127.0.0.1",
            "X-Original-Port": str(self.upstream_port),
            "X-Original-Scheme": "http",
            "X-Proxy-Token": "wrong-secret",
        }
        status, _, data = _proxy_request(self.proxy_port, "GET", "/test", headers)
        self.assertEqual(status, 403)

    def test_secret_not_forwarded_upstream(self):
        """X-Proxy-Token is stripped before forwarding to upstream."""
        headers = {
            "X-Original-Host": "127.0.0.1",
            "X-Original-Port": str(self.upstream_port),
            "X-Original-Scheme": "http",
            "X-Proxy-Token": self.secret,
        }
        status, _, data = _proxy_request(self.proxy_port, "GET", "/test", headers)
        echo = json.loads(data)
        self.assertNotIn("X-Proxy-Token", echo["headers"])


class TestCredentialProxyUnmatched(unittest.TestCase):
    """Requests to domains without credential rules."""

    @classmethod
    def setUpClass(cls):
        cls.upstream_server, cls.upstream_port = _start_mock_upstream(EchoHandler)
        cls.rules = [
            {"domain": "only-this-domain.example", "headers": {"Authorization": "Bearer secret"}},
        ]
        cls.proxy_proc, cls.proxy_port = _start_credential_proxy(cls.rules)

    @classmethod
    def tearDownClass(cls):
        _stop_proxy(cls.proxy_proc)
        cls.upstream_server.shutdown()

    def test_unmatched_domain_no_injection(self):
        """Requests to unmatched domains are forwarded without header injection."""
        headers = {
            "X-Original-Host": "127.0.0.1",
            "X-Original-Port": str(self.upstream_port),
            "X-Original-Scheme": "http",
        }
        status, _, data = _proxy_request(self.proxy_port, "GET", "/test", headers)
        self.assertEqual(status, 200)
        echo = json.loads(data)
        self.assertNotIn("Authorization", echo["headers"])


class TestCredentialProxyStreaming(unittest.TestCase):
    """Tests chunked/streaming responses."""

    @classmethod
    def setUpClass(cls):
        cls.upstream_server, cls.upstream_port = _start_mock_upstream(ChunkedHandler)
        cls.rules = [{"domain": "127.0.0.1", "headers": {}}]
        cls.proxy_proc, cls.proxy_port = _start_credential_proxy(cls.rules)

    @classmethod
    def tearDownClass(cls):
        _stop_proxy(cls.proxy_proc)
        cls.upstream_server.shutdown()

    def test_chunked_response(self):
        """Chunked upstream responses are re-chunked to the client."""
        headers = {
            "X-Original-Host": "127.0.0.1",
            "X-Original-Port": str(self.upstream_port),
            "X-Original-Scheme": "http",
        }
        status, resp_headers, data = _proxy_request(
            self.proxy_port, "GET", "/stream", headers)
        self.assertEqual(status, 200)
        body = data.decode()
        self.assertIn("chunk-0", body)
        self.assertIn("chunk-1", body)
        self.assertIn("chunk-2", body)


class TestCredentialProxyStartup(unittest.TestCase):
    """Tests for startup behavior."""

    def test_empty_rules_exits(self):
        """Proxy exits with error when CREDENTIAL_PROXY_RULES is empty."""
        env = os.environ.copy()
        env["CREDENTIAL_PROXY_RULES"] = "[]"
        proc = subprocess.Popen(
            [sys.executable, "credential-proxy.py"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )
        proc.wait(timeout=5)
        self.assertNotEqual(proc.returncode, 0)
        stderr = proc.stderr.read().decode()
        self.assertIn("empty", stderr.lower())

    def test_invalid_json_exits(self):
        """Proxy exits with error when CREDENTIAL_PROXY_RULES is invalid JSON."""
        env = os.environ.copy()
        env["CREDENTIAL_PROXY_RULES"] = "not-json"
        proc = subprocess.Popen(
            [sys.executable, "credential-proxy.py"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )
        proc.wait(timeout=5)
        self.assertNotEqual(proc.returncode, 0)

    def test_sigterm_shutdown(self):
        """Proxy shuts down cleanly on SIGTERM."""
        rules = [{"domain": "test.example", "headers": {}}]
        proc, port = _start_credential_proxy(rules)
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=2)
            self.fail("Proxy did not shut down within 3 seconds of SIGTERM")
        self.assertEqual(proc.returncode, 0)


class TestBuildCredentialRules(unittest.TestCase):
    """Test the _claude_vm_build_credential_rules shell function."""

    def _build_rules(self, anthropic_token, repos_json):
        result = subprocess.run(
            ["bash", "-c",
             f'source claude-vm.sh 2>/dev/null && '
             f"_claude_vm_build_credential_rules '{anthropic_token}' '{repos_json}'"],
            capture_output=True, text=True, timeout=15,
        )
        self.assertEqual(result.returncode, 0, f"stderr: {result.stderr}")
        return json.loads(result.stdout)

    def test_single_repo(self):
        """Single repo produces per-repo rules + fallback (4 github rules)."""
        repos = json.dumps({"owner/repo": "ghu_test"})
        rules = self._build_rules("", repos)
        # 2 per-repo (github.com + api.github.com) + 2 fallback = 4
        self.assertEqual(len(rules), 4)
        # Per-repo rules have path_prefix
        repo_rules = [r for r in rules if r.get("path_prefix")]
        self.assertEqual(len(repo_rules), 2)

    def test_multi_repo(self):
        """Multiple repos produce per-repo rules with different tokens."""
        repos = json.dumps({"org1/repo1": "token1", "org2/repo2": "token2"})
        rules = self._build_rules("", repos)
        # 2 repos * 2 rules each + 2 fallback = 6
        self.assertEqual(len(rules), 6)
        # Check different path prefixes
        git_rules = [r for r in rules if r["domain"] == "github.com" and r.get("path_prefix")]
        prefixes = [r["path_prefix"] for r in git_rules]
        self.assertIn("/org1/repo1", prefixes)
        self.assertIn("/org2/repo2", prefixes)

    def test_multi_repo_different_tokens(self):
        """Each repo gets its own token injected."""
        repos = json.dumps({"org1/repo1": "token_AAA", "org2/repo2": "token_BBB"})
        rules = self._build_rules("", repos)
        api_rules = {r["path_prefix"]: r for r in rules
                     if r["domain"] == "api.github.com" and r.get("path_prefix")}
        self.assertEqual(api_rules["/repos/org1/repo1"]["headers"]["Authorization"],
                         "token token_AAA")
        self.assertEqual(api_rules["/repos/org2/repo2"]["headers"]["Authorization"],
                         "token token_BBB")

    def test_both_tokens(self):
        """Both Anthropic and GitHub tokens produce correct rules."""
        repos = json.dumps({"owner/repo": "ghu_test"})
        rules = self._build_rules("sk-ant-test", repos)
        # 1 anthropic + 2 per-repo + 2 fallback = 5
        self.assertEqual(len(rules), 5)
        domains = [r["domain"] for r in rules]
        self.assertIn("api.anthropic.com", domains)
        self.assertIn("github.com", domains)
        self.assertIn("api.github.com", domains)

    def test_anthropic_only(self):
        """Only Anthropic token produces 1 rule."""
        rules = self._build_rules("sk-ant-test", "")
        self.assertEqual(len(rules), 1)
        self.assertEqual(rules[0]["domain"], "api.anthropic.com")
        self.assertEqual(rules[0]["headers"]["Authorization"], "Bearer sk-ant-test")
        self.assertEqual(rules[0]["headers"]["anthropic-beta"], "oauth-2025-04-20")

    def test_github_basic_auth_encoding(self):
        """GitHub token is base64-encoded as Basic auth for git."""
        repos = json.dumps({"owner/repo": "ghu_test"})
        rules = self._build_rules("", repos)
        github_rule = next(r for r in rules
                           if r["domain"] == "github.com" and r.get("path_prefix"))
        auth = github_rule["headers"]["Authorization"]
        self.assertTrue(auth.startswith("Basic "))
        decoded = base64.b64decode(auth.split(" ", 1)[1]).decode()
        self.assertEqual(decoded, "x-access-token:ghu_test")

    def test_github_api_token_auth(self):
        """api.github.com uses token auth."""
        repos = json.dumps({"owner/repo": "ghu_test"})
        rules = self._build_rules("", repos)
        api_rule = next(r for r in rules
                        if r["domain"] == "api.github.com" and r.get("path_prefix"))
        self.assertEqual(api_rule["headers"]["Authorization"], "token ghu_test")

    def test_fallback_rules(self):
        """Domain-level fallback rules have no path_prefix."""
        repos = json.dumps({"owner/repo": "ghu_test"})
        rules = self._build_rules("", repos)
        fallback = [r for r in rules if not r.get("path_prefix")]
        self.assertEqual(len(fallback), 2)
        fallback_domains = {r["domain"] for r in fallback}
        self.assertEqual(fallback_domains, {"github.com", "api.github.com"})

    def test_no_tokens(self):
        """No tokens produce empty rules."""
        rules = self._build_rules("", "")
        self.assertEqual(len(rules), 0)


class TestCredentialProxyPathMatching(unittest.TestCase):
    """Tests path-prefix-based credential matching for multi-repo support."""

    @classmethod
    def setUpClass(cls):
        cls.upstream_server, cls.upstream_port = _start_mock_upstream(EchoHandler)
        cls.rules = [
            # Per-repo rules with path_prefix
            {
                "domain": "127.0.0.1",
                "path_prefix": "/org1/repo1",
                "headers": {"Authorization": "token token-for-repo1"},
            },
            {
                "domain": "127.0.0.1",
                "path_prefix": "/org2/repo2",
                "headers": {"Authorization": "token token-for-repo2"},
            },
            # Domain-level fallback (no path_prefix)
            {
                "domain": "127.0.0.1",
                "headers": {"Authorization": "token fallback-token"},
            },
        ]
        cls.proxy_proc, cls.proxy_port = _start_credential_proxy(cls.rules)

    @classmethod
    def tearDownClass(cls):
        _stop_proxy(cls.proxy_proc)
        cls.upstream_server.shutdown()

    def _request(self, path):
        headers = {
            "X-Original-Host": "127.0.0.1",
            "X-Original-Port": str(self.upstream_port),
            "X-Original-Scheme": "http",
        }
        return _proxy_request(self.proxy_port, "GET", path, headers)

    def test_repo1_gets_repo1_token(self):
        """Requests to /org1/repo1/... get repo1's token."""
        status, _, data = self._request("/org1/repo1/info/refs?service=git-upload-pack")
        echo = json.loads(data)
        self.assertEqual(echo["headers"]["Authorization"], "token token-for-repo1")

    def test_repo2_gets_repo2_token(self):
        """Requests to /org2/repo2/... get repo2's token."""
        status, _, data = self._request("/org2/repo2/info/refs?service=git-upload-pack")
        echo = json.loads(data)
        self.assertEqual(echo["headers"]["Authorization"], "token token-for-repo2")

    def test_unknown_path_gets_fallback(self):
        """Requests to unknown paths get the fallback token."""
        status, _, data = self._request("/other/repo/info/refs")
        echo = json.loads(data)
        self.assertEqual(echo["headers"]["Authorization"], "token fallback-token")

    def test_root_path_gets_fallback(self):
        """Requests to / get the fallback token."""
        status, _, data = self._request("/user")
        echo = json.loads(data)
        self.assertEqual(echo["headers"]["Authorization"], "token fallback-token")


class TestMitmproxyAddon(unittest.TestCase):
    """Tests for the mitmproxy addon request() function logic.

    We don't import mitmproxy (not installed on host), but we test
    the addon logic by simulating the flow object.
    """

    def test_addon_rewrites_flow(self):
        """Addon sets X-Original-* headers and rewrites host/scheme/port."""
        # Simulate what the addon does without importing mitmproxy
        result = subprocess.run(
            [sys.executable, "-c", """
import os, types, sys

# Mock mitmproxy.http module
mock_http = types.ModuleType("mitmproxy.http")

class Headers(dict):
    def __setitem__(self, key, value):
        super().__setitem__(key, value)

class Request:
    def __init__(self):
        self.host = "api.anthropic.com"
        self.port = 443
        self.scheme = "https"
        self.headers = Headers()

class HTTPFlow:
    def __init__(self):
        self.request = Request()

mock_http.HTTPFlow = HTTPFlow
sys.modules["mitmproxy"] = types.ModuleType("mitmproxy")
sys.modules["mitmproxy.http"] = mock_http

os.environ["CREDENTIAL_PROXY_HOST"] = "host.lima.internal"
os.environ["CREDENTIAL_PROXY_PORT"] = "12345"
os.environ["CREDENTIAL_PROXY_DOMAINS"] = "api.anthropic.com,github.com"

# Import the addon
sys.path.insert(0, ".")
# Manually exec to avoid import caching issues
with open("mitmproxy-addon.py") as f:
    code = f.read()
ns = {"__name__": "mitmproxy_addon"}
exec(compile(code, "mitmproxy-addon.py", "exec"), ns)

# Test: configured domain gets redirected
flow = HTTPFlow()
ns["request"](flow)

assert flow.request.headers["X-Original-Host"] == "api.anthropic.com"
assert flow.request.headers["X-Original-Port"] == "443"
assert flow.request.headers["X-Original-Scheme"] == "https"
assert flow.request.scheme == "http"
assert flow.request.host == "host.lima.internal"
assert flow.request.port == 12345
print("ALL_PASSED")
"""],
            capture_output=True, text=True, timeout=5,
        )
        self.assertIn("ALL_PASSED", result.stdout, f"stderr: {result.stderr}")

    def test_addon_different_port(self):
        """Addon preserves non-443 port in X-Original-Port."""
        result = subprocess.run(
            [sys.executable, "-c", """
import os, types, sys

mock_http = types.ModuleType("mitmproxy.http")
class Headers(dict):
    pass
class Request:
    def __init__(self):
        self.host = "github.com"
        self.port = 8443
        self.scheme = "https"
        self.headers = Headers()
class HTTPFlow:
    def __init__(self):
        self.request = Request()
mock_http.HTTPFlow = HTTPFlow
sys.modules["mitmproxy"] = types.ModuleType("mitmproxy")
sys.modules["mitmproxy.http"] = mock_http

os.environ["CREDENTIAL_PROXY_HOST"] = "proxy-host"
os.environ["CREDENTIAL_PROXY_PORT"] = "9999"
os.environ["CREDENTIAL_PROXY_DOMAINS"] = "github.com,api.github.com"

sys.path.insert(0, ".")
with open("mitmproxy-addon.py") as f:
    code = f.read()
ns = {"__name__": "mitmproxy_addon"}
exec(compile(code, "mitmproxy-addon.py", "exec"), ns)

flow = HTTPFlow()
ns["request"](flow)

assert flow.request.headers["X-Original-Port"] == "8443"
assert flow.request.port == 9999
assert flow.request.host == "proxy-host"
print("ALL_PASSED")
"""],
            capture_output=True, text=True, timeout=5,
        )
        self.assertIn("ALL_PASSED", result.stdout, f"stderr: {result.stderr}")


    def test_addon_injects_secret(self):
        """Addon injects X-Proxy-Token when CREDENTIAL_PROXY_SECRET is set."""
        result = subprocess.run(
            [sys.executable, "-c", """
import os, types, sys

mock_http = types.ModuleType("mitmproxy.http")
class Headers(dict):
    pass
class Request:
    def __init__(self):
        self.host = "api.anthropic.com"
        self.port = 443
        self.scheme = "https"
        self.headers = Headers()
class HTTPFlow:
    def __init__(self):
        self.request = Request()
mock_http.HTTPFlow = HTTPFlow
sys.modules["mitmproxy"] = types.ModuleType("mitmproxy")
sys.modules["mitmproxy.http"] = mock_http

os.environ["CREDENTIAL_PROXY_HOST"] = "host.lima.internal"
os.environ["CREDENTIAL_PROXY_PORT"] = "12345"
os.environ["CREDENTIAL_PROXY_SECRET"] = "my-secret-token"
os.environ["CREDENTIAL_PROXY_DOMAINS"] = "api.anthropic.com"

sys.path.insert(0, ".")
with open("mitmproxy-addon.py") as f:
    code = f.read()
ns = {"__name__": "mitmproxy_addon"}
exec(compile(code, "mitmproxy-addon.py", "exec"), ns)

flow = HTTPFlow()
ns["request"](flow)

assert flow.request.headers["X-Proxy-Token"] == "my-secret-token"
print("ALL_PASSED")
"""],
            capture_output=True, text=True, timeout=5,
        )
        self.assertIn("ALL_PASSED", result.stdout, f"stderr: {result.stderr}")

    def test_addon_no_secret_no_header(self):
        """Addon does not inject X-Proxy-Token when secret is empty."""
        result = subprocess.run(
            [sys.executable, "-c", """
import os, types, sys

mock_http = types.ModuleType("mitmproxy.http")
class Headers(dict):
    pass
class Request:
    def __init__(self):
        self.host = "api.anthropic.com"
        self.port = 443
        self.scheme = "https"
        self.headers = Headers()
class HTTPFlow:
    def __init__(self):
        self.request = Request()
mock_http.HTTPFlow = HTTPFlow
sys.modules["mitmproxy"] = types.ModuleType("mitmproxy")
sys.modules["mitmproxy.http"] = mock_http

os.environ["CREDENTIAL_PROXY_HOST"] = "host.lima.internal"
os.environ["CREDENTIAL_PROXY_PORT"] = "12345"
os.environ.pop("CREDENTIAL_PROXY_SECRET", None)
os.environ["CREDENTIAL_PROXY_DOMAINS"] = "api.anthropic.com"

sys.path.insert(0, ".")
with open("mitmproxy-addon.py") as f:
    code = f.read()
ns = {"__name__": "mitmproxy_addon"}
exec(compile(code, "mitmproxy-addon.py", "exec"), ns)

flow = HTTPFlow()
ns["request"](flow)

assert "X-Proxy-Token" not in flow.request.headers
print("ALL_PASSED")
"""],
            capture_output=True, text=True, timeout=5,
        )
        self.assertIn("ALL_PASSED", result.stdout, f"stderr: {result.stderr}")


    def test_addon_skips_non_configured_domain(self):
        """Addon does not redirect requests to non-configured domains."""
        result = subprocess.run(
            [sys.executable, "-c", """
import os, types, sys

mock_http = types.ModuleType("mitmproxy.http")
class Headers(dict):
    pass
class Request:
    def __init__(self):
        self.host = "example.com"
        self.port = 443
        self.scheme = "https"
        self.headers = Headers()
class HTTPFlow:
    def __init__(self):
        self.request = Request()
mock_http.HTTPFlow = HTTPFlow
sys.modules["mitmproxy"] = types.ModuleType("mitmproxy")
sys.modules["mitmproxy.http"] = mock_http

os.environ["CREDENTIAL_PROXY_HOST"] = "host.lima.internal"
os.environ["CREDENTIAL_PROXY_PORT"] = "12345"
os.environ["CREDENTIAL_PROXY_DOMAINS"] = "api.github.com,github.com"

sys.path.insert(0, ".")
with open("mitmproxy-addon.py") as f:
    code = f.read()
ns = {"__name__": "mitmproxy_addon"}
exec(compile(code, "mitmproxy-addon.py", "exec"), ns)

flow = HTTPFlow()
ns["request"](flow)

# Non-configured domain should NOT be redirected
assert flow.request.host == "example.com", f"host was changed to {flow.request.host}"
assert flow.request.port == 443
assert flow.request.scheme == "https"
assert "X-Original-Host" not in flow.request.headers
print("ALL_PASSED")
"""],
            capture_output=True, text=True, timeout=5,
        )
        self.assertIn("ALL_PASSED", result.stdout, f"stderr: {result.stderr}")


if __name__ == "__main__":
    unittest.main()
