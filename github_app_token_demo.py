#!/usr/bin/env python3
"""
GitHub App User Token Generator

Generates repo-scoped GitHub user access tokens via the device flow.
Only requires the GitHub App's Client ID (no secrets needed).

Usage:
  # Print instructions to create the GitHub App (one-time setup)
  python3 github_app_token_demo.py create-app

  # Generate a user access token via device flow
  python3 github_app_token_demo.py user-token --client-id Iv23liXXXXXX

  # Scope the token to a single repo (writes only)
  python3 github_app_token_demo.py user-token --client-id Iv23liXXXXXX --repo wirenboard/agent-vm

Add -v/--verbose to any command for detailed HTTP logging.
"""

import argparse
import json
import os
import sys
import re
import time
import urllib.parse
import urllib.request
import urllib.error

TARGET_OWNER = os.environ.get("TARGET_OWNER", "wirenboard")
TARGET_REPO = os.environ.get("TARGET_REPO", "agent-vm")

VERBOSE = False

# Retry settings for transient GitHub errors (5xx)
MAX_RETRIES = 3
RETRY_DELAYS = [1, 2, 4]
ERROR_LOG_DIR = os.environ.get("GITHUB_TOKEN_LOG_DIR", ".")


def verbose(msg):
    if VERBOSE:
        print(f"  [verbose] {msg}", file=sys.stderr)


def _looks_like_html(body_str):
    """Check if a response body is HTML."""
    prefix = body_str[:256].lstrip()
    return prefix.startswith(("<", "<!DOCTYPE", "<!doctype"))


def _save_error_body(status, body_str, context=""):
    """Save an error response body to a file for debugging. Returns the path."""
    ts = time.strftime("%Y%m%d-%H%M%S")
    filename = f"github-token-error-{ts}-{status}.html"
    path = os.path.join(ERROR_LOG_DIR, filename)
    try:
        with open(path, "w") as f:
            if context:
                f.write(f"<!-- {context} -->\n")
            f.write(body_str)
        verbose(f"saved error response to {path}")
    except OSError as e:
        verbose(f"failed to save error response: {e}")
        path = None
    return path


def _copy_to_clipboard(text):
    """Try to copy text to the system clipboard. Fails silently."""
    import subprocess
    import shutil
    for cmd in (["pbcopy"], ["xclip", "-selection", "clipboard"], ["xsel", "--clipboard", "--input"], ["wl-copy"]):
        if shutil.which(cmd[0]):
            try:
                subprocess.run(cmd, input=text.encode(), check=True, timeout=5)
                verbose(f"copied to clipboard via {cmd[0]}")
                return True
            except (subprocess.SubprocessError, OSError):
                pass
    verbose("no clipboard tool found")
    return False


def parse_repo(repo_str):
    """Parse a repo URL/slug into (owner, name). Accepts:
    - owner/name
    - https://github.com/owner/name
    - git@github.com:owner/name.git
    """
    # git@github.com:owner/name.git
    m = re.match(r'git@github\.com:([^/]+)/([^/]+?)(?:\.git)?$', repo_str)
    if m:
        return m.group(1), m.group(2)
    # https://github.com/owner/name[.git]
    m = re.match(r'https?://github\.com/([^/]+)/([^/]+?)(?:\.git)?(?:/.*)?$', repo_str)
    if m:
        return m.group(1), m.group(2)
    # owner/name
    m = re.match(r'^([^/]+)/([^/]+)$', repo_str)
    if m:
        return m.group(1), m.group(2)
    print(f"Error: cannot parse repo: {repo_str}", file=sys.stderr)
    sys.exit(1)


def resolve_repo_id(owner, name):
    """Resolve a repo's numeric ID. Tries public API first, falls back to gh CLI."""
    import subprocess

    url = f"https://api.github.com/repos/{owner}/{name}"
    req = urllib.request.Request(url)
    req.add_header("Accept", "application/vnd.github+json")
    verbose(f"Resolving repo ID: {owner}/{name}")
    try:
        with urllib.request.urlopen(req) as resp:
            data = json.loads(resp.read().decode())
            repo_id = data["id"]
            verbose(f"Resolved {owner}/{name} -> {repo_id}")
            return str(repo_id)
    except urllib.error.HTTPError:
        verbose(f"Public API failed, trying gh CLI...")
        try:
            result = subprocess.run(
                ["gh", "api", f"repos/{owner}/{name}", "--jq", ".id"],
                capture_output=True, text=True, check=True,
            )
            repo_id = result.stdout.strip()
            verbose(f"Resolved {owner}/{name} -> {repo_id} (via gh)")
            return repo_id
        except (subprocess.CalledProcessError, FileNotFoundError) as e:
            print(f"Error: cannot resolve repo {owner}/{name}", file=sys.stderr)
            print(f"  Public API returned 404 and gh CLI failed: {e}", file=sys.stderr)
            print(f"  Use --repository-id to pass the numeric ID directly", file=sys.stderr)
            sys.exit(1)


# --- App creation helper ---

APP_MANIFEST = {
    "name": "WB Agent VM Token Generator",
    "url": "https://github.com/wirenboard/agent-vm",
    "hook_attributes": {"active": False},
    "redirect_url": "",
    "public": False,
    "default_permissions": {"contents": "write"},
    "default_events": [],
}


def cmd_create_app():
    """Print instructions for creating the GitHub App via the UI."""
    print("=" * 60)
    print("GitHub App Creation Instructions")
    print("=" * 60)
    print()
    print("1. Go to: https://github.com/organizations/wirenboard/settings/apps/new")
    print("   (Or for personal account: https://github.com/settings/apps/new)")
    print()
    print("2. Fill in the following settings:")
    print(f"   - GitHub App name: {APP_MANIFEST['name']}")
    print(f"   - Homepage URL: {APP_MANIFEST['url']}")
    print("   - Uncheck 'Active' under Webhook")
    print("   - Under 'Repository permissions':")
    print("     - Contents: Read & write")
    print("   - Under 'Where can this GitHub App be installed?':")
    print("     - Select 'Only on this account'")
    print()
    print("3. Click 'Create GitHub App'")
    print()
    print("4. Note the Client ID (starts with Iv...)")
    print()
    print("5. Install the app:")
    print("   - Click 'Install App' in the left sidebar")
    print("   - Choose the target organization/account")
    print("   - Select repositories to grant access to")
    print()
    print("6. Generate a token:")
    print(f"   python3 {sys.argv[0]} user-token --client-id <CLIENT_ID> --repo <owner/repo>")
    print()


# --- GitHub API helper ---


def github_api(method, path, token, token_type="Bearer", body=None):
    """Make a GitHub API request with retry on 5xx errors."""
    url = f"https://api.github.com{path}"
    data = json.dumps(body).encode() if body else None

    verbose(f">>> {method} {url}")
    if body:
        verbose(f">>> Body: {json.dumps(body)}")

    last_error = None
    for attempt in range(MAX_RETRIES + 1):
        if attempt > 0:
            delay = RETRY_DELAYS[min(attempt - 1, len(RETRY_DELAYS) - 1)]
            verbose(f"retry {attempt}/{MAX_RETRIES} after {delay}s")
            time.sleep(delay)

        req = urllib.request.Request(url, data=data, method=method)
        req.add_header("Accept", "application/vnd.github+json")
        req.add_header("Authorization", f"{token_type} {token}")
        req.add_header("X-GitHub-Api-Version", "2022-11-28")
        if data:
            req.add_header("Content-Type", "application/json")

        try:
            with urllib.request.urlopen(req) as resp:
                resp_body = resp.read().decode()
                verbose(f"<<< {resp.status} {resp.reason}")
                verbose(f"<<< Body: {resp_body[:500]}{'...' if len(resp_body) > 500 else ''}")
                return json.loads(resp_body)
        except urllib.error.HTTPError as e:
            error_body = e.read().decode()
            verbose(f"<<< {e.code} {e.reason}")
            verbose(f"<<< Body: {error_body}")
            if e.code >= 500:
                if _looks_like_html(error_body):
                    _save_error_body(e.code, error_body, f"{method} {url} attempt {attempt + 1}")
                    last_error = f"{e.code} {e.reason} (HTML error page saved to disk)"
                else:
                    last_error = f"{e.code} {e.reason}"
                print(f"GitHub API error: {e.code} {e.reason} (attempt {attempt + 1}/{MAX_RETRIES + 1}, retrying...)", file=sys.stderr)
                continue
            # Non-5xx error — don't retry
            print(f"GitHub API error: {e.code} {e.reason}", file=sys.stderr)
            print(f"URL: {url}", file=sys.stderr)
            if _looks_like_html(error_body):
                saved = _save_error_body(e.code, error_body, f"{method} {url}")
                print(f"Response body saved to: {saved}", file=sys.stderr)
            else:
                print(f"Response: {error_body}", file=sys.stderr)
            sys.exit(1)
        except urllib.error.URLError as e:
            last_error = str(e.reason)
            print(f"GitHub API error: {last_error} (attempt {attempt + 1}/{MAX_RETRIES + 1}, retrying...)", file=sys.stderr)
            continue

    print(f"GitHub API error: {last_error} (all {MAX_RETRIES + 1} attempts failed)", file=sys.stderr)
    print(f"URL: {url}", file=sys.stderr)
    sys.exit(1)


def verify_token(token, token_type="token"):
    """Verify the token works by reading the target repo."""
    repo = github_api(
        "GET",
        f"/repos/{TARGET_OWNER}/{TARGET_REPO}",
        token,
        token_type=token_type,
    )
    return repo


# --- Token cache ---

DEFAULT_CACHE_DIR = os.path.expanduser("~/.cache/claude-vm")


def _cache_path(cache_dir, client_id, owner, repo):
    """Return the cache file path for a given client/repo combo."""
    safe = f"{owner}_{repo}".replace("/", "_")
    return os.path.join(cache_dir, f"github-token-{safe}.json")


def load_cached_token(cache_dir, client_id, owner, repo):
    """Try to load a valid token from cache. Returns token string or None."""
    path = _cache_path(cache_dir, client_id, owner, repo)
    try:
        with open(path, "r") as f:
            data = json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return None

    expires_at = data.get("expires_at", 0)
    # 5 minute margin
    if time.time() < expires_at - 300:
        verbose(f"Using cached token (expires in {int(expires_at - time.time())}s)")
        return data.get("access_token")

    # Token expired — try refresh
    refresh_token = data.get("refresh_token")
    if not refresh_token:
        verbose("Cached token expired, no refresh token")
        return None

    print("Cached token expired, refreshing...", file=sys.stderr)
    try:
        resp = device_flow_request(
            "https://github.com/login/oauth/access_token",
            {
                "client_id": client_id,
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
            },
        )
    except SystemExit:
        print("Refresh failed, falling back to browser login", file=sys.stderr)
        return None

    if resp.get("error"):
        print(f"Refresh failed ({resp.get('error')}), falling back to browser login",
              file=sys.stderr)
        return None

    token = resp.get("access_token")
    if not token:
        return None

    print("Token refreshed successfully", file=sys.stderr)
    save_cached_token(
        cache_dir, client_id, owner, repo,
        token,
        resp.get("refresh_token", refresh_token),
        resp.get("expires_in", 28800),
    )
    return token


def save_cached_token(cache_dir, client_id, owner, repo,
                      access_token, refresh_token, expires_in):
    """Save token to cache file."""
    path = _cache_path(cache_dir, client_id, owner, repo)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    data = {
        "access_token": access_token,
        "refresh_token": refresh_token,
        "expires_at": time.time() + (int(expires_in) if expires_in != "never" else 86400),
    }
    with open(path, "w") as f:
        json.dump(data, f)
    # Restrict permissions — token file
    os.chmod(path, 0o600)
    verbose(f"Token cached to {path}")


# --- Device flow (user access token) ---


def device_flow_request(url, params):
    """POST to GitHub's OAuth endpoints (form-encoded, JSON response).
    Retries on 5xx errors; saves HTML error pages to disk."""
    encoded_data = urllib.parse.urlencode(params).encode()

    verbose(f">>> POST {url}")
    verbose(f">>> Body: {urllib.parse.urlencode(params)}")

    last_error = None
    for attempt in range(MAX_RETRIES + 1):
        if attempt > 0:
            delay = RETRY_DELAYS[min(attempt - 1, len(RETRY_DELAYS) - 1)]
            verbose(f"retry {attempt}/{MAX_RETRIES} after {delay}s")
            time.sleep(delay)

        req = urllib.request.Request(url, data=encoded_data, method="POST")
        req.add_header("Accept", "application/json")

        try:
            with urllib.request.urlopen(req) as resp:
                resp_body = resp.read().decode()
                verbose(f"<<< {resp.status} {resp.reason}")
                verbose(f"<<< Body: {resp_body[:500]}{'...' if len(resp_body) > 500 else ''}")
                return json.loads(resp_body)
        except urllib.error.HTTPError as e:
            error_body = e.read().decode()
            verbose(f"<<< {e.code} {e.reason}")
            verbose(f"<<< Body: {error_body}")
            if e.code >= 500:
                if _looks_like_html(error_body):
                    _save_error_body(e.code, error_body, f"POST {url} attempt {attempt + 1}")
                    last_error = f"{e.code} {e.reason} (HTML error page saved to disk)"
                else:
                    last_error = f"{e.code} {e.reason}"
                print(f"  GitHub returned {e.code} (attempt {attempt + 1}/{MAX_RETRIES + 1}, retrying...)", file=sys.stderr)
                continue
            # Non-5xx error — don't retry
            print(f"GitHub OAuth error: {e.code} {e.reason}", file=sys.stderr)
            if _looks_like_html(error_body):
                saved = _save_error_body(e.code, error_body, f"POST {url}")
                print(f"Response body saved to: {saved}", file=sys.stderr)
            else:
                print(f"Response: {error_body}", file=sys.stderr)
            sys.exit(1)
        except urllib.error.URLError as e:
            last_error = str(e.reason)
            print(f"  Connection error: {last_error} (attempt {attempt + 1}/{MAX_RETRIES + 1}, retrying...)", file=sys.stderr)
            continue

    print(f"GitHub OAuth failed: {last_error} (all {MAX_RETRIES + 1} attempts failed)", file=sys.stderr)
    sys.exit(1)


def cmd_user_token(client_id, repository_id=None, token_only=False,
                   cache_dir=None):
    """Generate a user access token via the device flow."""
    out = sys.stderr if token_only else sys.stdout

    # Try cached token first
    if cache_dir:
        cached = load_cached_token(cache_dir, client_id, TARGET_OWNER, TARGET_REPO)
        if cached:
            print(f"Using cached token for {TARGET_OWNER}/{TARGET_REPO}", file=out)
            if token_only:
                print(cached)
            else:
                print("=" * 60)
                print(f"GITHUB_TOKEN={cached}")
                print("=" * 60)
            return

    print("Requesting device code...", file=out)
    resp = device_flow_request(
        "https://github.com/login/device/code",
        {"client_id": client_id},
    )

    device_code = resp["device_code"]
    user_code = resp["user_code"]
    verification_uri = resp["verification_uri"]
    expires_in = resp["expires_in"]
    interval = resp.get("interval", 5)

    verbose(f"Device code: {device_code}")
    verbose(f"Polling interval: {interval}s, expires in: {expires_in}s")

    # Copy the user code to clipboard if possible
    copied = _copy_to_clipboard(user_code)
    clip_hint = " (copied to clipboard)" if copied else ""

    print(file=out)
    print("=" * 60, file=out)
    print(f"  Open:  {verification_uri}", file=out)
    print(f"  Enter: {user_code}{clip_hint}", file=out)
    print("=" * 60, file=out)
    print(file=out)
    print(f"Waiting for authorization (expires in {expires_in}s)...", file=out)

    # Poll for the token
    poll_count = 0
    while True:
        time.sleep(interval)
        poll_count += 1
        verbose(f"Polling attempt #{poll_count}...")
        poll_params = {
            "client_id": client_id,
            "device_code": device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
        }
        if repository_id:
            poll_params["repository_id"] = repository_id
        token_resp = device_flow_request(
            "https://github.com/login/oauth/access_token",
            poll_params,
        )

        error = token_resp.get("error")
        if error == "authorization_pending":
            verbose("Status: authorization_pending")
            continue
        elif error == "slow_down":
            interval = token_resp.get("interval", interval + 5)
            verbose(f"Status: slow_down, new interval: {interval}s")
            continue
        elif error == "expired_token":
            print("Error: Device code expired. Please try again.", file=sys.stderr)
            sys.exit(1)
        elif error == "access_denied":
            print("Error: Authorization was denied by the user.", file=sys.stderr)
            sys.exit(1)
        elif error:
            print(f"Error: {error} - {token_resp.get('error_description', '')}", file=sys.stderr)
            sys.exit(1)

        # Success
        token = token_resp["access_token"]
        token_type = token_resp.get("token_type", "bearer")
        expires = token_resp.get("expires_in", "never")
        refresh = token_resp.get("refresh_token", "")
        verbose(f"Token type: {token_type}")
        verbose(f"Full token response keys: {list(token_resp.keys())}")
        break

    print("Authorization successful!", file=out)
    print(file=out)

    if expires != "never":
        print(f"Token expires in: {expires}s", file=out)
    if refresh:
        print(f"Refresh token: {refresh}", file=out)
    print(file=out)

    print("Verifying token...", file=out)
    repo = verify_token(token, token_type="Bearer")
    print(f"Verified: can access {repo['full_name']}", file=out)
    print(file=out)

    # Cache the token
    if cache_dir:
        save_cached_token(cache_dir, client_id, TARGET_OWNER, TARGET_REPO,
                          token, refresh, expires)

    if token_only:
        print(token)
    else:
        print("=" * 60)
        print(f"GITHUB_TOKEN={token}")
        print("=" * 60)


# --- CLI ---


def main():
    global VERBOSE

    parser = argparse.ArgumentParser(
        description="Generate repo-scoped GitHub user tokens via a GitHub App"
    )
    parser.add_argument("-v", "--verbose", action="store_true",
                        help="Enable verbose HTTP logging to stderr")
    sub = parser.add_subparsers(dest="command")

    sub.add_parser("create-app", help="Print instructions to create the GitHub App")

    user_parser = sub.add_parser("user-token", help="Generate a user access token via device flow")
    user_parser.add_argument("--client-id", required=True, help="GitHub App Client ID (Iv23li...)")
    user_parser.add_argument("--repo", type=str, default=None,
                             help="Repository to scope the token to (owner/name, URL, or git@github.com:owner/name.git)")
    user_parser.add_argument("--repository-id", type=str, default=None,
                             help="Numeric repo ID (use for private repos where --repo can't resolve)")
    user_parser.add_argument("--token-only", action="store_true",
                             help="Print only the token to stdout (status messages go to stderr)")
    user_parser.add_argument("--cache-dir", type=str, default=None,
                             help="Directory to cache tokens (default: no caching)")

    args = parser.parse_args()
    VERBOSE = args.verbose

    if args.command == "create-app":
        cmd_create_app()
    elif args.command == "user-token":
        global TARGET_OWNER, TARGET_REPO
        repository_id = args.repository_id
        if args.repo:
            TARGET_OWNER, TARGET_REPO = parse_repo(args.repo)
            if not repository_id:
                repository_id = resolve_repo_id(TARGET_OWNER, TARGET_REPO)
        cmd_user_token(args.client_id, repository_id=repository_id,
                       token_only=args.token_only,
                       cache_dir=args.cache_dir)
    else:
        parser.print_help()
        sys.exit(1)


if __name__ == "__main__":
    main()
