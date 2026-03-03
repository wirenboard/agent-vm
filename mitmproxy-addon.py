"""
mitmproxy addon for agent VMs.

Intercepts HTTPS requests to configured domains and forwards them as plain
HTTP to the host-side credential proxy, which injects real auth tokens.

Requests to non-configured domains pass through to the real upstream unchanged.

Usage:
    CREDENTIAL_PROXY_HOST=host.lima.internal \
    CREDENTIAL_PROXY_PORT=12345 \
    CREDENTIAL_PROXY_DOMAINS=api.anthropic.com,github.com,api.github.com \
        mitmdump -s mitmproxy-addon.py
"""

import os
from mitmproxy import http

PROXY_HOST = os.environ.get("CREDENTIAL_PROXY_HOST", "host.lima.internal")
PROXY_PORT = int(os.environ.get("CREDENTIAL_PROXY_PORT", "0"))
PROXY_SECRET = os.environ.get("CREDENTIAL_PROXY_SECRET", "")
PROXY_DOMAINS = set(
    d.strip()
    for d in os.environ.get("CREDENTIAL_PROXY_DOMAINS", "").split(",")
    if d.strip()
)


def request(flow: http.HTTPFlow) -> None:
    # Only redirect configured domains to the credential proxy
    if flow.request.host not in PROXY_DOMAINS:
        return

    # Add original destination info as headers
    flow.request.headers["X-Original-Host"] = flow.request.host
    flow.request.headers["X-Original-Port"] = str(flow.request.port)
    flow.request.headers["X-Original-Scheme"] = flow.request.scheme

    # Add shared secret for cross-VM isolation
    if PROXY_SECRET:
        flow.request.headers["X-Proxy-Token"] = PROXY_SECRET

    # Redirect to host-side credential proxy over plain HTTP
    flow.request.scheme = "http"
    flow.request.host = PROXY_HOST
    flow.request.port = PROXY_PORT
