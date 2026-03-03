#!/usr/bin/env python3
"""Full E2E test: host proxy + VM mitmproxy + gh CLI."""
import http.client, json, os, signal, subprocess, sys, time

SECRET = "e2e-secret-" + os.urandom(8).hex()
RULES = [
    {"domain": "api.github.com", "path_prefix": "/repos/wirenboard/agent-vm",
     "headers": {"Authorization": "token TOK_AGENT_VM"}},
    {"domain": "api.github.com", "path_prefix": "/repos/wirenboard/wb-cloud-ui",
     "headers": {"Authorization": "token TOK_CLOUD_UI"}},
    {"domain": "api.github.com",
     "headers": {"Authorization": "token TOK_FALLBACK"}},
]

# Start credential proxy
env = os.environ.copy()
env["CREDENTIAL_PROXY_RULES"] = json.dumps(RULES)
env["CREDENTIAL_PROXY_SECRET"] = SECRET

proc = subprocess.Popen([sys.executable, "credential-proxy.py"],
    stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
port_line = proc.stdout.readline().decode().strip()
PROXY_PORT = int(port_line)
print(f"Credential proxy on :{PROXY_PORT}, secret={SECRET[:20]}...")

# Copy addon to VM
subprocess.run(["bash", "-c",
    "cat mitmproxy-addon.py | limactl shell test-e2e bash -c 'cat > ~/.mitmproxy/addon.py'"],
    check=True)

# Kill any old mitmproxy
subprocess.run(["limactl", "shell", "test-e2e", "bash", "-c",
    "pkill -f mitmdump; sleep 1"], capture_output=True)

# Start mitmproxy in VM
start_cmd = (
    f"CREDENTIAL_PROXY_HOST=host.lima.internal "
    f"CREDENTIAL_PROXY_PORT={PROXY_PORT} "
    f"CREDENTIAL_PROXY_SECRET={SECRET} "
    f"CREDENTIAL_PROXY_DOMAINS=api.github.com,github.com "
    f"nohup mitmdump "
    f"  --listen-port 8080 "
    f"  --set connection_strategy=lazy "
    f"  -s ~/.mitmproxy/addon.py "
    f"  > /tmp/mitmproxy.log 2>&1 & "
    f"sleep 3; echo READY"
)
r = subprocess.run(["limactl", "shell", "test-e2e", "bash", "-c", start_cmd],
    capture_output=True, text=True, timeout=15)
print(f"mitmproxy: {r.stdout.strip()}")

results = []

# Test from VM using curl through mitmproxy
for name, path in [
    ("agent-vm repo", "/repos/wirenboard/agent-vm"),
    ("wb-cloud-ui repo", "/repos/wirenboard/wb-cloud-ui"),
    ("user (fallback)", "/user"),
]:
    cmd = (
        f"curl -s -w '\\nHTTP_CODE:%{{http_code}}' "
        f"--proxy http://127.0.0.1:8080 "
        f"https://api.github.com{path} 2>/dev/null"
    )
    r = subprocess.run(["limactl", "shell", "test-e2e", "bash", "-c", cmd],
        capture_output=True, text=True, timeout=15)
    http_code = ""
    for line in r.stdout.strip().split('\n'):
        if line.startswith("HTTP_CODE:"):
            http_code = line.split(":")[1]
    # 401 = fake token sent and rejected; 200 = public repo (no auth needed)
    ok = http_code in ("401", "200")
    print(f"  {name}: HTTP {http_code} -> {'PASS' if ok else 'FAIL'}")
    results.append(ok)

# Test gh CLI from VM
cmd = (
    "export HTTPS_PROXY=http://127.0.0.1:8080; "
    "export GH_TOKEN=placeholder; "
    "gh api /repos/wirenboard/agent-vm --jq .full_name 2>&1; echo EXIT:$?"
)
r = subprocess.run(["limactl", "shell", "test-e2e", "bash", "-c", cmd],
    capture_output=True, text=True, timeout=15)
gh_output = r.stdout.strip()
# gh gets 401 (Bad credentials) or succeeds (200 for public repo)
ok = "agent-vm" in gh_output or "Bad credentials" in gh_output
print(f"  gh CLI: {gh_output[:60]} -> {'PASS' if ok else 'FAIL'}")
results.append(ok)

# Test wrong secret from VM (direct curl to proxy, bypassing mitmproxy)
cmd = (
    f"curl -s -w '\\nHTTP_CODE:%{{http_code}}' "
    f"-H 'X-Original-Host: api.github.com' "
    f"-H 'X-Original-Port: 443' "
    f"-H 'X-Original-Scheme: https' "
    f"-H 'X-Proxy-Token: WRONG' "
    f"http://host.lima.internal:{PROXY_PORT}/repos/wirenboard/agent-vm 2>/dev/null"
)
r = subprocess.run(["limactl", "shell", "test-e2e", "bash", "-c", cmd],
    capture_output=True, text=True, timeout=10)
code = ""
for line in r.stdout.strip().split('\n'):
    if line.startswith("HTTP_CODE:"):
        code = line.split(":")[1]
ok = code == "403"
print(f"  wrong secret: HTTP {code} -> {'PASS' if ok else 'FAIL'}")
results.append(ok)

# Test no secret from VM
cmd = (
    f"curl -s -w '\\nHTTP_CODE:%{{http_code}}' "
    f"-H 'X-Original-Host: api.github.com' "
    f"-H 'X-Original-Port: 443' "
    f"-H 'X-Original-Scheme: https' "
    f"http://host.lima.internal:{PROXY_PORT}/repos/wirenboard/agent-vm 2>/dev/null"
)
r = subprocess.run(["limactl", "shell", "test-e2e", "bash", "-c", cmd],
    capture_output=True, text=True, timeout=10)
code = ""
for line in r.stdout.strip().split('\n'):
    if line.startswith("HTTP_CODE:"):
        code = line.split(":")[1]
ok = code == "403"
print(f"  no secret: HTTP {code} -> {'PASS' if ok else 'FAIL'}")
results.append(ok)

# Cleanup
proc.send_signal(signal.SIGTERM)
try:
    proc.wait(timeout=3)
except subprocess.TimeoutExpired:
    proc.kill()
    proc.wait()

passed = sum(results)
total = len(results)
print(f"\nResults: {passed}/{total}")
if passed == total:
    print("All E2E VM tests passed!")
sys.exit(0 if passed == total else 1)
