#!/usr/bin/env python3
"""Tests for the repo scope enforcement in github-mcp-proxy.py."""

import json
import os
import sys

# Set env before importing
os.environ["GITHUB_MCP_TOKEN"] = "fake-token"
os.environ["GITHUB_MCP_OWNER"] = "wirenboard"
os.environ["GITHUB_MCP_REPO"] = "agent-vm"
os.environ["GITHUB_MCP_PROXY_DEBUG"] = "1"

# Reload module to pick up env
if "github-mcp-proxy" in sys.modules:
    del sys.modules["github-mcp-proxy"]

# Import the proxy module
import importlib.util
spec = importlib.util.spec_from_file_location(
    "proxy", os.path.join(os.path.dirname(__file__), "github-mcp-proxy.py"))
proxy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(proxy)


def make_tool_call(tool_name, arguments):
    """Build an MCP tools/call request body."""
    return json.dumps({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": arguments,
        }
    }).encode()


def check(label, body_bytes, expect_blocked=False, expect_query=None,
          expect_unchanged=False):
    """Run enforce_repo_scope and check the result."""
    result, err = proxy.enforce_repo_scope(body_bytes)
    if expect_blocked:
        assert err is not None, f"FAIL [{label}]: expected BLOCKED but got through"
        print(f"  PASS [{label}]: blocked with: {err}")
    else:
        assert err is None, f"FAIL [{label}]: expected ALLOWED but blocked: {err}"
        if expect_query is not None:
            req = json.loads(result)
            actual_query = req["params"]["arguments"]["query"]
            assert actual_query == expect_query, \
                f"FAIL [{label}]: expected query={expect_query!r}, got {actual_query!r}"
            print(f"  PASS [{label}]: allowed, query={actual_query!r}")
        elif expect_unchanged:
            assert result == body_bytes, \
                f"FAIL [{label}]: body was re-serialized when it shouldn't have been"
            print(f"  PASS [{label}]: allowed, body unchanged")
        else:
            print(f"  PASS [{label}]: allowed")


print("=== Test: existing owner/repo check still works ===")
check("correct repo",
      make_tool_call("create_branch", {"owner": "wirenboard", "repo": "agent-vm", "branch": "test"}),
      expect_unchanged=True)
check("wrong repo",
      make_tool_call("create_branch", {"owner": "wirenboard", "repo": "exposure-probe", "branch": "test"}),
      expect_blocked=True)
check("wrong owner",
      make_tool_call("create_branch", {"owner": "evil-org", "repo": "agent-vm", "branch": "test"}),
      expect_blocked=True)

print()
print("=== Test: search tools get repo scope injected ===")
check("search_code no scope",
      make_tool_call("search_code", {"query": "def main"}),
      expect_query="repo:wirenboard/agent-vm def main")
check("search_code correct scope",
      make_tool_call("search_code", {"query": "repo:wirenboard/agent-vm def main"}),
      expect_query="repo:wirenboard/agent-vm def main")
check("search_code wrong scope",
      make_tool_call("search_code", {"query": "repo:wirenboard/exposure-probe def main"}),
      expect_blocked=True)
check("search_repositories no scope",
      make_tool_call("search_repositories", {"query": "language:python"}),
      expect_query="repo:wirenboard/agent-vm language:python")
check("search_issues no scope",
      make_tool_call("search_issues", {"query": "is:open bug"}),
      expect_query="repo:wirenboard/agent-vm is:open bug")
check("search_pull_requests wrong scope",
      make_tool_call("search_pull_requests", {"query": "repo:evil/repo is:open"}),
      expect_blocked=True)

print()
print("=== Test: org:/user: qualifiers blocked in search queries ===")
check("search_code with org:",
      make_tool_call("search_code", {"query": "org:wirenboard def main"}),
      expect_blocked=True)
check("search_code with user:",
      make_tool_call("search_code", {"query": "user:evgeny-boger password"}),
      expect_blocked=True)
check("search_issues with org:",
      make_tool_call("search_issues", {"query": "org:evil-corp is:open"}),
      expect_blocked=True)

print()
print("=== Test: search_users and search_orgs blocked ===")
check("search_users",
      make_tool_call("search_users", {"query": "john"}),
      expect_blocked=True)
check("search_orgs",
      make_tool_call("search_orgs", {"query": "wirenboard"}),
      expect_blocked=True)

print()
print("=== Test: unscoped tools allowed ===")
check("get_me",
      make_tool_call("get_me", {}),
      expect_unchanged=True)

print()
print("=== Test: org-level tools blocked ===")
check("get_teams",
      make_tool_call("get_teams", {"user": "someone"}),
      expect_blocked=True)
check("get_team_members",
      make_tool_call("get_team_members", {"org": "wirenboard", "team_slug": "devs"}),
      expect_blocked=True)
check("list_issue_types",
      make_tool_call("list_issue_types", {"owner": "wirenboard"}),
      expect_blocked=True)

print()
print("=== Test: unknown tools blocked (default-deny) ===")
check("totally_fake_tool",
      make_tool_call("totally_fake_tool", {"owner": "wirenboard", "repo": "agent-vm"}),
      expect_blocked=True)
check("future_dangerous_tool",
      make_tool_call("future_dangerous_tool", {"query": "secrets"}),
      expect_blocked=True)

print()
print("=== Test: optional owner/repo gets injected ===")
check("list_issues missing owner/repo",
      make_tool_call("list_issues", {"state": "OPEN"}))
# Verify the injected values
result, _ = proxy.enforce_repo_scope(
    make_tool_call("list_issues", {"state": "OPEN"}))
req = json.loads(result)
assert req["params"]["arguments"]["owner"] == "wirenboard", "owner not injected"
assert req["params"]["arguments"]["repo"] == "agent-vm", "repo not injected"
print("  PASS [list_issues injection]: owner/repo injected correctly")

print()
print("=== Test: body not re-serialized when unmodified ===")
# When owner/repo are already correct, body should pass through as-is
check("get_file_contents unchanged",
      make_tool_call("get_file_contents",
                     {"owner": "wirenboard", "repo": "agent-vm", "path": "/"}),
      expect_unchanged=True)

print()
print("=== Test: non-tool-call methods pass through ===")
init_body = json.dumps({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                         "params": {"protocolVersion": "2024-11-05"}}).encode()
result, err = proxy.enforce_repo_scope(init_body)
assert err is None and result == init_body
print("  PASS [initialize]: passed through unchanged")

print()
print("All tests passed!")
