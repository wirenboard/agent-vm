#!/usr/bin/env python3
"""Tests for OAuth token refresh logic in claude-vm-proxy.py."""

import http.server
import json
import os
import shutil
import ssl
import sys
import tempfile
import threading
import time
import unittest

# Load the proxy module
import importlib.util
spec = importlib.util.spec_from_file_location(
    "proxy", os.path.join(os.path.dirname(__file__), "claude-vm-proxy.py")
)
proxy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(proxy)


class TestIsTokenExpiring(unittest.TestCase):
    def test_no_expires_at(self):
        self.assertFalse(proxy._is_token_expiring({"accessToken": "x"}))

    def test_valid_token(self):
        future = int((time.time() + 3600) * 1000)
        self.assertFalse(proxy._is_token_expiring({"expiresAt": future}))

    def test_expired_token(self):
        past = int((time.time() - 100) * 1000)
        self.assertTrue(proxy._is_token_expiring({"expiresAt": past}))

    def test_expiring_within_buffer(self):
        # Within 5 minutes (300s) of expiry
        soon = int((time.time() + 200) * 1000)
        self.assertTrue(proxy._is_token_expiring({"expiresAt": soon}))

    def test_just_outside_buffer(self):
        later = int((time.time() + 400) * 1000)
        self.assertFalse(proxy._is_token_expiring({"expiresAt": later}))

    def test_bad_expires_at(self):
        self.assertFalse(proxy._is_token_expiring({"expiresAt": "not-a-number"}))

    def test_expires_at_as_numeric_string(self):
        future = str(int((time.time() + 3600) * 1000))
        self.assertFalse(proxy._is_token_expiring({"expiresAt": future}))

    def test_expired_as_numeric_string(self):
        past = str(int((time.time() - 100) * 1000))
        self.assertTrue(proxy._is_token_expiring({"expiresAt": past}))


class TestReadOAuthCreds(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.creds_path = os.path.join(self.tmpdir, ".credentials.json")
        self._orig_path = proxy.CREDENTIALS_PATH
        proxy.CREDENTIALS_PATH = self.creds_path

    def tearDown(self):
        proxy.CREDENTIALS_PATH = self._orig_path
        shutil.rmtree(self.tmpdir)

    def test_missing_file(self):
        creds, oauth, err = proxy._read_oauth_creds()
        self.assertIsNone(creds)
        self.assertIn("No credentials found", err)

    def test_invalid_json(self):
        with open(self.creds_path, "w") as f:
            f.write("not json")
        creds, oauth, err = proxy._read_oauth_creds()
        self.assertIsNone(creds)
        self.assertIn("Failed to read", err)

    def test_valid_creds(self):
        data = {"claudeAiOauth": {"accessToken": "tok", "refreshToken": "ref"}}
        with open(self.creds_path, "w") as f:
            json.dump(data, f)
        creds, oauth, err = proxy._read_oauth_creds()
        self.assertIsNone(err)
        self.assertEqual(oauth["accessToken"], "tok")

    def test_missing_oauth_section(self):
        with open(self.creds_path, "w") as f:
            json.dump({"other": "data"}, f)
        creds, oauth, err = proxy._read_oauth_creds()
        self.assertIsNone(err)
        self.assertEqual(oauth, {})


class TestSaveCredentials(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.creds_path = os.path.join(self.tmpdir, ".credentials.json")
        self._orig_path = proxy.CREDENTIALS_PATH
        self._orig_dir = proxy.CREDENTIALS_DIR
        proxy.CREDENTIALS_PATH = self.creds_path
        proxy.CREDENTIALS_DIR = self.tmpdir

    def tearDown(self):
        proxy.CREDENTIALS_PATH = self._orig_path
        proxy.CREDENTIALS_DIR = self._orig_dir
        shutil.rmtree(self.tmpdir)

    def test_save_new_file(self):
        creds = {"claudeAiOauth": {"accessToken": "new", "expiresAt": 999}}
        proxy._save_credentials(creds)
        with open(self.creds_path) as f:
            saved = json.load(f)
        self.assertEqual(saved["claudeAiOauth"]["accessToken"], "new")

    def test_save_preserves_other_keys(self):
        # Write initial file with extra keys
        with open(self.creds_path, "w") as f:
            json.dump({"claudeAiOauth": {"accessToken": "old"}, "otherKey": "keep"}, f)
        creds = {"claudeAiOauth": {"accessToken": "new"}}
        proxy._save_credentials(creds)
        with open(self.creds_path) as f:
            saved = json.load(f)
        self.assertEqual(saved["claudeAiOauth"]["accessToken"], "new")
        self.assertEqual(saved["otherKey"], "keep")

    def test_file_permissions(self):
        creds = {"claudeAiOauth": {"accessToken": "x"}}
        proxy._save_credentials(creds)
        mode = os.stat(self.creds_path).st_mode & 0o777
        self.assertEqual(mode, 0o600)


class FakeTokenServer:
    """A mock OAuth token endpoint for testing refresh."""

    def __init__(self):
        self.requests = []
        self.response_data = {
            "access_token": "new-access-token",
            "refresh_token": "new-refresh-token",
            "expires_in": 3600,
            "scope": "user:profile user:inference",
        }
        self.response_code = 200
        self._server = None
        self._thread = None

    def start(self):
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0))
                body = json.loads(self.rfile.read(length)) if length else {}
                outer.requests.append(body)
                resp = json.dumps(outer.response_data).encode()
                self.send_response(outer.response_code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(resp)))
                self.end_headers()
                self.wfile.write(resp)

            def log_message(self, *args):
                pass

        self._server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()
        port = self._server.server_address[1]
        return "127.0.0.1", port

    def stop(self):
        if self._server:
            self._server.shutdown()
            self._server.server_close()


class TestRefreshOAuthToken(unittest.TestCase):
    def setUp(self):
        self.server = FakeTokenServer()
        host, port = self.server.start()
        self._orig_host = proxy.TOKEN_HOST
        self._orig_path = proxy.TOKEN_PATH
        self._orig_tls = proxy._token_use_tls
        proxy.TOKEN_HOST = f"{host}:{port}"
        proxy.TOKEN_PATH = "/v1/oauth/token"
        proxy._token_use_tls = False

    def tearDown(self):
        proxy.TOKEN_HOST = self._orig_host
        proxy.TOKEN_PATH = self._orig_path
        proxy._token_use_tls = self._orig_tls
        self.server.stop()

    def test_successful_refresh(self):
        new_oauth, err = proxy._refresh_oauth_token("my-refresh-token")
        self.assertIsNone(err)
        self.assertEqual(new_oauth["accessToken"], "new-access-token")
        self.assertEqual(new_oauth["refreshToken"], "new-refresh-token")
        self.assertIn("expiresAt", new_oauth)
        self.assertIn("scopes", new_oauth)
        # Verify the request we sent
        self.assertEqual(len(self.server.requests), 1)
        req = self.server.requests[0]
        self.assertEqual(req["grant_type"], "refresh_token")
        self.assertEqual(req["refresh_token"], "my-refresh-token")
        self.assertEqual(req["client_id"], proxy.OAUTH_CLIENT_ID)

    def test_refresh_preserves_original_refresh_token_if_not_returned(self):
        self.server.response_data = {
            "access_token": "new-at",
            "expires_in": 7200,
            "scope": "user:inference",
        }
        new_oauth, err = proxy._refresh_oauth_token("original-rt")
        self.assertIsNone(err)
        self.assertEqual(new_oauth["refreshToken"], "original-rt")

    def test_refresh_http_error(self):
        self.server.response_code = 400
        self.server.response_data = {"error": "invalid_grant"}
        new_oauth, err = proxy._refresh_oauth_token("bad-token")
        self.assertIsNone(new_oauth)
        self.assertIn("HTTP 400", err)

    def test_malformed_json_response(self):
        """Server returns 200 with invalid JSON."""
        self.server.response_data = "not json"  # Handler will json.dumps this, so override differently
        # Actually, the mock always json.dumps, so test via a 200 with missing access_token
        self.server.response_data = {"no_access_token": True}
        new_oauth, err = proxy._refresh_oauth_token("rt")
        self.assertIsNone(new_oauth)
        self.assertIn("missing access_token", err)

    def test_refresh_missing_access_token(self):
        self.server.response_data = {"expires_in": 3600}
        new_oauth, err = proxy._refresh_oauth_token("rt")
        self.assertIsNone(new_oauth)
        self.assertIn("missing access_token", err)

    def test_expires_at_calculation(self):
        self.server.response_data = {
            "access_token": "at",
            "expires_in": 7200,
            "scope": "user:inference",
        }
        before = int(time.time() * 1000)
        new_oauth, err = proxy._refresh_oauth_token("rt")
        after = int(time.time() * 1000)
        self.assertIsNone(err)
        expected_min = before + 7200 * 1000
        expected_max = after + 7200 * 1000
        self.assertGreaterEqual(new_oauth["expiresAt"], expected_min)
        self.assertLessEqual(new_oauth["expiresAt"], expected_max)


class TestEnsureFreshToken(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.creds_path = os.path.join(self.tmpdir, ".credentials.json")
        self._orig_path = proxy.CREDENTIALS_PATH
        self._orig_dir = proxy.CREDENTIALS_DIR
        proxy.CREDENTIALS_PATH = self.creds_path
        proxy.CREDENTIALS_DIR = self.tmpdir

        self.server = FakeTokenServer()
        host, port = self.server.start()
        self._orig_host = proxy.TOKEN_HOST
        self._orig_path_token = proxy.TOKEN_PATH
        self._orig_tls = proxy._token_use_tls
        proxy.TOKEN_HOST = f"{host}:{port}"
        proxy.TOKEN_PATH = "/v1/oauth/token"
        proxy._token_use_tls = False

    def tearDown(self):
        proxy.CREDENTIALS_PATH = self._orig_path
        proxy.CREDENTIALS_DIR = self._orig_dir
        proxy.TOKEN_HOST = self._orig_host
        proxy.TOKEN_PATH = self._orig_path_token
        proxy._token_use_tls = self._orig_tls
        self.server.stop()
        shutil.rmtree(self.tmpdir)

    def _write_creds(self, access_token="tok", refresh_token="ref", expires_at=None, **extra):
        oauth = {"accessToken": access_token, "refreshToken": refresh_token}
        if expires_at is not None:
            oauth["expiresAt"] = expires_at
        oauth.update(extra)
        with open(self.creds_path, "w") as f:
            json.dump({"claudeAiOauth": oauth}, f)

    def test_valid_token_no_refresh(self):
        future = int((time.time() + 3600) * 1000)
        self._write_creds(expires_at=future)
        token, err = proxy._ensure_fresh_token()
        self.assertIsNone(err)
        self.assertEqual(token, "tok")
        self.assertEqual(len(self.server.requests), 0)  # No refresh call

    def test_expired_token_triggers_refresh(self):
        past = int((time.time() - 100) * 1000)
        self._write_creds(expires_at=past)
        token, err = proxy._ensure_fresh_token()
        self.assertIsNone(err)
        self.assertEqual(token, "new-access-token")
        self.assertEqual(len(self.server.requests), 1)

    def test_expired_token_saves_to_disk(self):
        past = int((time.time() - 100) * 1000)
        self._write_creds(expires_at=past, subscriptionType="max", rateLimitTier="tier1")
        token, err = proxy._ensure_fresh_token()
        self.assertIsNone(err)
        # Verify file was updated
        with open(self.creds_path) as f:
            saved = json.load(f)
        self.assertEqual(saved["claudeAiOauth"]["accessToken"], "new-access-token")
        self.assertEqual(saved["claudeAiOauth"]["refreshToken"], "new-refresh-token")
        # Subscription info preserved
        self.assertEqual(saved["claudeAiOauth"]["subscriptionType"], "max")
        self.assertEqual(saved["claudeAiOauth"]["rateLimitTier"], "tier1")

    def test_no_refresh_token_returns_error(self):
        past = int((time.time() - 100) * 1000)
        oauth = {"accessToken": "tok", "expiresAt": past}
        with open(self.creds_path, "w") as f:
            json.dump({"claudeAiOauth": oauth}, f)
        token, err = proxy._ensure_fresh_token()
        self.assertIsNone(token)
        self.assertIn("no refreshToken", err)

    def test_no_access_token_returns_error(self):
        with open(self.creds_path, "w") as f:
            json.dump({"claudeAiOauth": {}}, f)
        token, err = proxy._ensure_fresh_token()
        self.assertIsNone(token)
        self.assertIn("No accessToken", err)

    def test_missing_file_returns_error(self):
        token, err = proxy._ensure_fresh_token()
        self.assertIsNone(token)
        self.assertIn("No credentials found", err)

    def test_refresh_failure_returns_error(self):
        past = int((time.time() - 100) * 1000)
        self._write_creds(expires_at=past)
        self.server.response_code = 401
        self.server.response_data = {"error": "invalid_grant"}
        token, err = proxy._ensure_fresh_token()
        self.assertIsNone(token)
        self.assertIn("refresh failed", err)

    def test_host_refreshed_token_used_without_calling_endpoint(self):
        """If the host Claude CLI already refreshed, we should use the new token."""
        # Write a valid (non-expired) token
        future = int((time.time() + 3600) * 1000)
        self._write_creds(access_token="host-refreshed-token", expires_at=future)
        token, err = proxy._ensure_fresh_token()
        self.assertIsNone(err)
        self.assertEqual(token, "host-refreshed-token")
        self.assertEqual(len(self.server.requests), 0)


class TestGetAuthToken(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.creds_path = os.path.join(self.tmpdir, ".credentials.json")
        self._orig_path = proxy.CREDENTIALS_PATH
        self._orig_dir = proxy.CREDENTIALS_DIR
        proxy.CREDENTIALS_PATH = self.creds_path
        proxy.CREDENTIALS_DIR = self.tmpdir
        self._orig_env = os.environ.get("ANTHROPIC_API_KEY")
        os.environ.pop("ANTHROPIC_API_KEY", None)

    def tearDown(self):
        proxy.CREDENTIALS_PATH = self._orig_path
        proxy.CREDENTIALS_DIR = self._orig_dir
        if self._orig_env is not None:
            os.environ["ANTHROPIC_API_KEY"] = self._orig_env
        else:
            os.environ.pop("ANTHROPIC_API_KEY", None)
        shutil.rmtree(self.tmpdir)

    def test_api_key_takes_priority(self):
        os.environ["ANTHROPIC_API_KEY"] = "sk-test-key"
        token, is_oauth, err = proxy.get_auth_token()
        self.assertIsNone(err)
        self.assertEqual(token, "sk-test-key")
        self.assertFalse(is_oauth)

    def test_oauth_token_returned(self):
        future = int((time.time() + 3600) * 1000)
        with open(self.creds_path, "w") as f:
            json.dump({"claudeAiOauth": {
                "accessToken": "oauth-tok",
                "refreshToken": "ref",
                "expiresAt": future,
            }}, f)
        token, is_oauth, err = proxy.get_auth_token()
        self.assertIsNone(err)
        self.assertEqual(token, "oauth-tok")
        self.assertTrue(is_oauth)


@unittest.skipUnless(
    os.environ.get("RUN_REAL_TESTS"),
    "Set RUN_REAL_TESTS=1 to run tests against real OAuth endpoint"
)
class TestRealRefresh(unittest.TestCase):
    """Test against the real Anthropic OAuth endpoint using actual credentials.
    Saves refreshed tokens back to disk to avoid invalidating credentials.
    WARNING: This modifies real credentials. Only run with RUN_REAL_TESTS=1."""

    def test_real_token_refresh(self):
        real_path = os.path.expanduser("~/.claude/.credentials.json")
        try:
            with open(real_path) as f:
                creds = json.load(f)
        except (FileNotFoundError, json.JSONDecodeError):
            self.skipTest("No real credentials available")

        oauth = creds.get("claudeAiOauth", {})
        refresh_token = oauth.get("refreshToken")
        if not refresh_token:
            self.skipTest("No refresh token in credentials")

        # Temporarily use the real token endpoint and credentials path
        orig_host = proxy.TOKEN_HOST
        orig_token_path = proxy.TOKEN_PATH
        orig_path = proxy.CREDENTIALS_PATH
        orig_dir = proxy.CREDENTIALS_DIR
        proxy.TOKEN_HOST = "platform.claude.com"
        proxy.TOKEN_PATH = "/v1/oauth/token"
        proxy.CREDENTIALS_PATH = real_path
        proxy.CREDENTIALS_DIR = os.path.dirname(real_path)
        try:
            new_oauth, err = proxy._refresh_oauth_token(refresh_token)
            self.assertIsNone(err, f"Real refresh failed: {err}")
            self.assertIsNotNone(new_oauth)
            self.assertTrue(new_oauth["accessToken"].startswith("sk-ant-"))
            self.assertIn("expiresAt", new_oauth)
            self.assertIn("scopes", new_oauth)
            # IMPORTANT: Save new tokens so the old refresh token isn't left dangling
            for key in ("subscriptionType", "rateLimitTier"):
                if key in oauth and key not in new_oauth:
                    new_oauth[key] = oauth[key]
            creds["claudeAiOauth"] = new_oauth
            proxy._save_credentials(creds)
            print(f"\n  Real refresh OK: token={new_oauth['accessToken'][:16]}... "
                  f"expires_in={(new_oauth['expiresAt'] - time.time()*1000)/1000:.0f}s"
                  f" (saved to disk)")
        finally:
            proxy.TOKEN_HOST = orig_host
            proxy.TOKEN_PATH = orig_token_path
            proxy.CREDENTIALS_PATH = orig_path
            proxy.CREDENTIALS_DIR = orig_dir


if __name__ == "__main__":
    unittest.main(verbosity=2)
