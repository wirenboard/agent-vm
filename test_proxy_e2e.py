#!/usr/bin/env python3
"""End-to-end test for the proxy scope enforcement.

Starts a mock MCP server (upstream) and the proxy in front of it,
then sends requests through the proxy and verifies scoping.
"""

import http.server
import json
import os
import socketserver
import sys
import threading
import time
import urllib.request
import urllib.error

# --- Mock upstream MCP server ---
# Records what it receives so we can verify the proxy modified the request.

received_requests = []
received_headers = []


class MockMCPHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length) if length else b""
        req = json.loads(body) if body else {}
        received_requests.append(req)
        # Capture headers as a dict for inspection
        received_headers.append(dict(self.headers))

        # Return a mock MCP response
        result = {"content": [{"type": "text", "text": json.dumps({"mock": True})}]}
        resp = json.dumps({"jsonrpc": "2.0", "id": req.get("id"), "result": result})
        resp_bytes = resp.encode()

        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(resp_bytes)))
        self.end_headers()
        self.wfile.write(resp_bytes)

    def do_GET(self):
        self.do_POST()


def start_mock_server():
    server = socketserver.TCPServer(("127.0.0.1", 0), MockMCPHandler)
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, port


# --- Proxy setup ---

def start_proxy(upstream_host, upstream_port, extra_env=None):
    """Start the proxy pointing at our mock upstream."""
    # We need to patch the proxy module to use HTTP instead of HTTPS
    # and point at our mock server
    import importlib.util
    spec = importlib.util.spec_from_file_location(
        "proxy_mod", os.path.join(os.path.dirname(__file__), "github-mcp-proxy.py"))
    proxy = importlib.util.module_from_spec(spec)

    # Override env before loading
    os.environ["GITHUB_MCP_TOKEN"] = "test-token-broader-scope"
    os.environ["GITHUB_MCP_OWNER"] = "wirenboard"
    os.environ["GITHUB_MCP_REPO"] = "agent-vm"
    os.environ["GITHUB_MCP_PROXY_DEBUG"] = "1"
    # Clear optional env vars by default
    for key in ("GITHUB_MCP_TOOLSETS", "GITHUB_MCP_TOOLS", "GITHUB_MCP_READONLY"):
        os.environ.pop(key, None)
    if extra_env:
        os.environ.update(extra_env)

    spec.loader.exec_module(proxy)

    # Monkey-patch to use our mock server instead of GitHub
    proxy.MCP_HOST = upstream_host
    proxy.MCP_PORT = upstream_port
    proxy.MCP_PATH = "/"

    # Patch do_request to use HTTP instead of HTTPS
    original_do_request = proxy.GitHubMCPProxyHandler.do_request

    def patched_do_request(self):
        import http.client as hc
        # Store original HTTPSConnection and replace with HTTPConnection
        orig_https = hc.HTTPSConnection
        hc.HTTPSConnection = lambda host, port, **kw: hc.HTTPConnection(host, port, timeout=kw.get("timeout", 30))
        try:
            original_do_request(self)
        finally:
            hc.HTTPSConnection = orig_https

    proxy.GitHubMCPProxyHandler.do_request = patched_do_request
    proxy.GitHubMCPProxyHandler.do_GET = patched_do_request
    proxy.GitHubMCPProxyHandler.do_POST = patched_do_request

    server = socketserver.TCPServer(("127.0.0.1", 0), proxy.GitHubMCPProxyHandler)
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, port


def mcp_request(proxy_port, tool_name, arguments):
    """Send an MCP tools/call through the proxy."""
    body = json.dumps({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": tool_name, "arguments": arguments}
    }).encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{proxy_port}/mcp",
        data=body,
        headers={
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        },
    )
    try:
        with urllib.request.urlopen(req) as resp:
            return resp.status, json.loads(resp.read().decode())
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read().decode())


# --- Tests ---

def main():
    print("Starting mock upstream MCP server...")
    mock_server, mock_port = start_mock_server()
    print(f"  Mock server on port {mock_port}")

    print("Starting proxy...")
    proxy_server, proxy_port = start_proxy("127.0.0.1", mock_port)
    print(f"  Proxy on port {proxy_port}")
    time.sleep(0.2)

    passed = 0
    failed = 0

    def check(label, status, resp, expect_blocked=False, check_upstream_query=None):
        nonlocal passed, failed
        if expect_blocked:
            if status == 403 and "error" in resp:
                print(f"  PASS [{label}]: blocked - {resp['error']['message']}")
                passed += 1
            else:
                print(f"  FAIL [{label}]: expected 403 block, got {status}")
                failed += 1
        else:
            if status == 200:
                if check_upstream_query is not None:
                    last_req = received_requests[-1]
                    actual_query = last_req["params"]["arguments"]["query"]
                    if actual_query == check_upstream_query:
                        print(f"  PASS [{label}]: upstream saw query={actual_query!r}")
                        passed += 1
                    else:
                        print(f"  FAIL [{label}]: expected query={check_upstream_query!r}, got {actual_query!r}")
                        failed += 1
                else:
                    print(f"  PASS [{label}]: allowed (status {status})")
                    passed += 1
            else:
                print(f"  FAIL [{label}]: expected 200, got {status}: {resp}")
                failed += 1

    print()
    print("=== E2E: search_code with no scope gets repo injected ===")
    received_requests.clear()
    status, resp = mcp_request(proxy_port, "search_code", {"query": "def main"})
    check("search_code unscoped", status, resp,
          check_upstream_query="repo:wirenboard/agent-vm def main")

    print()
    print("=== E2E: search_code targeting wrong repo is BLOCKED ===")
    status, resp = mcp_request(proxy_port, "search_code",
                               {"query": "repo:wirenboard/exposure-probe secret"})
    check("search_code wrong repo", status, resp, expect_blocked=True)

    print()
    print("=== E2E: search_repositories gets repo scope injected ===")
    received_requests.clear()
    status, resp = mcp_request(proxy_port, "search_repositories",
                               {"query": "language:python"})
    check("search_repos unscoped", status, resp,
          check_upstream_query="repo:wirenboard/agent-vm language:python")

    print()
    print("=== E2E: create_branch on wrong repo is BLOCKED ===")
    status, resp = mcp_request(proxy_port, "create_branch",
                               {"owner": "wirenboard", "repo": "exposure-probe",
                                "branch": "evil-branch"})
    check("create_branch wrong repo", status, resp, expect_blocked=True)

    print()
    print("=== E2E: create_branch on correct repo passes through ===")
    status, resp = mcp_request(proxy_port, "create_branch",
                               {"owner": "wirenboard", "repo": "agent-vm",
                                "branch": "legit-branch"})
    check("create_branch correct repo", status, resp)

    print()
    print("=== E2E: get_me (unscoped tool) passes through ===")
    status, resp = mcp_request(proxy_port, "get_me", {})
    check("get_me", status, resp)

    print()
    print("=== E2E: get_teams (org tool) is BLOCKED ===")
    status, resp = mcp_request(proxy_port, "get_teams", {"user": "someone"})
    check("get_teams", status, resp, expect_blocked=True)

    print()
    print("=== E2E: search_users (not repo-scoped) is BLOCKED ===")
    status, resp = mcp_request(proxy_port, "search_users", {"query": "john"})
    check("search_users", status, resp, expect_blocked=True)

    print()
    print("=== E2E: org:/user: qualifiers in search queries BLOCKED ===")
    status, resp = mcp_request(proxy_port, "search_code",
                               {"query": "org:wirenboard def main"})
    check("search_code with org:", status, resp, expect_blocked=True)
    status, resp = mcp_request(proxy_port, "search_issues",
                               {"query": "user:someone is:open"})
    check("search_issues with user:", status, resp, expect_blocked=True)

    print()
    print("=== E2E: unknown tools BLOCKED (default-deny) ===")
    status, resp = mcp_request(proxy_port, "totally_new_tool",
                               {"owner": "wirenboard", "repo": "agent-vm"})
    check("unknown tool", status, resp, expect_blocked=True)

    print()
    print("=== E2E: list_issues without owner/repo gets them injected ===")
    received_requests.clear()
    status, resp = mcp_request(proxy_port, "list_issues", {"state": "OPEN"})
    check("list_issues no owner/repo", status, resp)
    # Verify upstream received the injected values
    last_req = received_requests[-1]
    args = last_req["params"]["arguments"]
    assert args["owner"] == "wirenboard", f"owner not injected: {args}"
    assert args["repo"] == "agent-vm", f"repo not injected: {args}"
    print(f"  PASS [list_issues injection verified]: owner={args['owner']}, repo={args['repo']}")
    passed += 1

    # --- Test tool-filtering headers ---
    # Stop first proxy, start a new one with TOOLSETS/TOOLS/READONLY
    proxy_server.shutdown()

    print()
    print("=== E2E: tool-filtering headers (X-MCP-Toolsets, X-MCP-Tools, X-MCP-Readonly) ===")
    proxy_server2, proxy_port2 = start_proxy("127.0.0.1", mock_port, extra_env={
        "GITHUB_MCP_TOOLSETS": "repos,issues",
        "GITHUB_MCP_TOOLS": "get_file_contents,issue_read",
        "GITHUB_MCP_READONLY": "1",
    })
    time.sleep(0.2)

    received_headers.clear()
    status, resp = mcp_request(proxy_port2, "get_file_contents",
                               {"owner": "wirenboard", "repo": "agent-vm", "path": "/"})
    if status == 200 and received_headers:
        h = received_headers[-1]
        checks = [
            ("X-MCP-Toolsets" in h and h["X-MCP-Toolsets"] == "repos,issues",
             "X-MCP-Toolsets"),
            ("X-MCP-Tools" in h and h["X-MCP-Tools"] == "get_file_contents,issue_read",
             "X-MCP-Tools"),
            ("X-MCP-Readonly" in h and h["X-MCP-Readonly"] == "true",
             "X-MCP-Readonly"),
        ]
        for ok, name in checks:
            if ok:
                print(f"  PASS [{name} header]: upstream received {name}={h.get(name)!r}")
                passed += 1
            else:
                print(f"  FAIL [{name} header]: not found or wrong value in upstream headers: {h}")
                failed += 1
    else:
        print(f"  FAIL [header injection]: request failed with status {status}")
        failed += 3

    # Verify VM can't override these headers
    received_headers.clear()
    body = json.dumps({
        "jsonrpc": "2.0", "id": 1,
        "method": "tools/call",
        "params": {"name": "get_file_contents",
                   "arguments": {"owner": "wirenboard", "repo": "agent-vm", "path": "/"}}
    }).encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{proxy_port2}/mcp",
        data=body,
        headers={
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "X-MCP-Toolsets": "repos,issues,pull_requests,actions,users,orgs",
            "X-MCP-Tools": "create_branch,push_files,delete_file",
            "X-MCP-Readonly": "false",
        },
    )
    try:
        with urllib.request.urlopen(req) as resp_obj:
            pass
    except urllib.error.HTTPError:
        pass

    if received_headers:
        h = received_headers[-1]
        # The proxy should strip the VM's values and inject its own
        vm_override_checks = [
            (h.get("X-MCP-Toolsets") == "repos,issues",
             "X-MCP-Toolsets not overridden by VM"),
            (h.get("X-MCP-Tools") == "get_file_contents,issue_read",
             "X-MCP-Tools not overridden by VM"),
            (h.get("X-MCP-Readonly") == "true",
             "X-MCP-Readonly not overridden by VM"),
        ]
        for ok, label in vm_override_checks:
            if ok:
                print(f"  PASS [{label}]")
                passed += 1
            else:
                print(f"  FAIL [{label}]: VM override leaked through: {h}")
                failed += 1
    else:
        print("  FAIL [VM override test]: no headers captured")
        failed += 3

    proxy_server2.shutdown()

    # --- Test defaults (toolsets + lockdown enabled, tools/readonly absent) ---
    print()
    print("=== E2E: default headers (toolsets + lockdown on, tools/readonly off) ===")
    proxy_server3, proxy_port3 = start_proxy("127.0.0.1", mock_port)
    time.sleep(0.2)

    received_headers.clear()
    status, resp = mcp_request(proxy_port3, "get_file_contents",
                               {"owner": "wirenboard", "repo": "agent-vm", "path": "/"})
    if status == 200 and received_headers:
        h = received_headers[-1]
        default_checks = [
            (h.get("X-MCP-Toolsets") == "repos,issues,pull_requests,git,labels",
             "X-MCP-Toolsets default"),
            ("X-MCP-Tools" not in h,
             "X-MCP-Tools absent by default"),
            ("X-MCP-Readonly" not in h,
             "X-MCP-Readonly absent by default"),
            (h.get("X-MCP-Lockdown") == "true",
             "X-MCP-Lockdown enabled by default"),
        ]
        for ok, label in default_checks:
            if ok:
                print(f"  PASS [{label}]: {h.get('X-MCP-Toolsets', '(absent)')}, lockdown={h.get('X-MCP-Lockdown', '(absent)')}")
                passed += 1
            else:
                print(f"  FAIL [{label}]: headers={h}")
                failed += 1
    else:
        print(f"  FAIL [defaults test]: request failed with status {status}")
        failed += 4

    # Verify VM can't inject X-MCP-Lockdown
    received_headers.clear()
    body = json.dumps({
        "jsonrpc": "2.0", "id": 1,
        "method": "tools/call",
        "params": {"name": "get_file_contents",
                   "arguments": {"owner": "wirenboard", "repo": "agent-vm", "path": "/"}}
    }).encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{proxy_port3}/mcp",
        data=body,
        headers={
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "X-MCP-Lockdown": "false",
        },
    )
    try:
        with urllib.request.urlopen(req) as resp_obj:
            pass
    except urllib.error.HTTPError:
        pass
    if received_headers:
        h = received_headers[-1]
        if h.get("X-MCP-Lockdown") == "true":
            print(f"  PASS [X-MCP-Lockdown not overridden by VM]")
            passed += 1
        else:
            print(f"  FAIL [X-MCP-Lockdown overridden by VM]: {h.get('X-MCP-Lockdown')}")
            failed += 1
    else:
        print("  FAIL [lockdown override test]: no headers captured")
        failed += 1

    proxy_server3.shutdown()

    # --- Test lockdown disabled ---
    print()
    print("=== E2E: lockdown disabled when GITHUB_MCP_LOCKDOWN=0 ===")
    proxy_server4, proxy_port4 = start_proxy("127.0.0.1", mock_port, extra_env={
        "GITHUB_MCP_LOCKDOWN": "0",
    })
    time.sleep(0.2)

    received_headers.clear()
    status, resp = mcp_request(proxy_port4, "get_file_contents",
                               {"owner": "wirenboard", "repo": "agent-vm", "path": "/"})
    if status == 200 and received_headers:
        h = received_headers[-1]
        if "X-MCP-Lockdown" not in h:
            print(f"  PASS [X-MCP-Lockdown absent when disabled]")
            passed += 1
        else:
            print(f"  FAIL [X-MCP-Lockdown should be absent]: {h.get('X-MCP-Lockdown')}")
            failed += 1
    else:
        print(f"  FAIL [lockdown disabled test]: request failed")
        failed += 1

    proxy_server4.shutdown()

    print()
    print(f"{'=' * 50}")
    print(f"Results: {passed} passed, {failed} failed")
    if failed:
        sys.exit(1)
    else:
        print("All end-to-end tests passed!")


if __name__ == "__main__":
    main()
