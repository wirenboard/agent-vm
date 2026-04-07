#!/usr/bin/env python3
"""
Obtain and cache a GitHub OAuth token for the Copilot API.

Uses the OpenCode OAuth App (Ov23li8tweQw6odWQebz) with read:user scope.
This app is the only one that grants access to the full Copilot model list
(Claude, Gemini, GPT-5, etc.).

Usage:
    python3 copilot_token.py <cache_file>

Exits 0 and prints the token to stdout on success.
Exits 1 on failure (device code expired, auth error, etc.).

The cache file is read first; if it contains a valid token it is used
without running the device flow. On success the token is written back to
the cache file for future invocations.
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request

CLIENT_ID = "Ov23li8tweQw6odWQebz"
SCOPE = "read:user"

# Bypass any HTTP proxy for GitHub auth endpoints
_opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def _post(url, params):
    data = json.dumps(params).encode()
    req = urllib.request.Request(
        url, data=data,
        headers={"Accept": "application/json", "Content-Type": "application/json"},
    )
    with _opener.open(req, timeout=10) as r:
        return json.load(r)


def _load_cached(cache_file):
    """Return cached token string if present and valid, else None."""
    try:
        with open(cache_file) as f:
            token = json.load(f).get("access_token", "")
    except (FileNotFoundError, json.JSONDecodeError):
        return None
    if not token:
        return None

    # Validate against GitHub
    req = urllib.request.Request("https://api.github.com/user")
    req.add_header("Authorization", f"token {token}")
    req.add_header("Accept", "application/vnd.github+json")
    try:
        with _opener.open(req, timeout=10) as r:
            r.read()
    except urllib.error.HTTPError as e:
        if e.code == 401:
            print("  Cached Copilot token rejected (401), discarding", file=sys.stderr)
            try:
                os.unlink(cache_file)
            except OSError:
                pass
            return None
    except Exception:
        pass  # Network error — keep token, let it fail later

    return token


def _save(cache_file, token):
    os.makedirs(os.path.dirname(cache_file), exist_ok=True)
    with open(cache_file, "w") as f:
        json.dump({"access_token": token}, f)
    os.chmod(cache_file, 0o600)


def _device_flow(cache_file):
    """Run OAuth device flow. Prints token to stdout and exits 0 on success."""
    resp = _post(
        "https://github.com/login/device/code",
        {"client_id": CLIENT_ID, "scope": SCOPE},
    )
    interval = resp.get("interval", 5)

    print(file=sys.stderr)
    print("  " + "=" * 50, file=sys.stderr)
    print(f"    Open:  https://github.com/login/device", file=sys.stderr)
    print(f"    Enter: {resp['user_code']}", file=sys.stderr)
    print("  " + "=" * 50, file=sys.stderr)
    print(file=sys.stderr)
    print(f"  Waiting for authorization (expires in {resp['expires_in']}s)...", file=sys.stderr)

    deadline = time.time() + resp["expires_in"]
    while time.time() < deadline:
        time.sleep(interval)
        try:
            token_resp = _post(
                "https://github.com/login/oauth/access_token",
                {
                    "client_id": CLIENT_ID,
                    "device_code": resp["device_code"],
                    "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                },
            )
        except Exception:
            continue

        if "access_token" in token_resp:
            token = token_resp["access_token"]
            _save(cache_file, token)
            print("  Authorization successful!", file=sys.stderr)
            print(token)
            sys.exit(0)

        error = token_resp.get("error", "")
        if error == "slow_down":
            interval = token_resp.get("interval", interval + 5)
        elif error == "authorization_pending":
            continue
        else:
            print(f"  Error: {error}", file=sys.stderr)
            sys.exit(1)

    print("  Error: device code expired", file=sys.stderr)
    sys.exit(1)


def main():
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <cache_file>", file=sys.stderr)
        sys.exit(1)

    cache_file = sys.argv[1]

    token = _load_cached(cache_file)
    if token:
        print("Using cached Copilot token", file=sys.stderr)
        print(token)
        sys.exit(0)

    print("Requesting GitHub token for Copilot API access...", file=sys.stderr)
    _device_flow(cache_file)


if __name__ == "__main__":
    main()
