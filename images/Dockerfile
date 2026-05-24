# agent-vm v1 base image.
#
# Debian 13 slim + the three AI coding agents (Claude Code, OpenCode, Codex)
# and the minimum dev tooling they need to be useful. Heavier extras
# (Docker-in-VM, Chromium, LSPs, MCP servers) are deliberately deferred.
#
# Built and pushed to a host-local registry by images/build.sh; consumed by
# microsandbox via `localhost:5000/agent-vm:latest` with `insecure()`.

FROM debian:13-slim

ENV DEBIAN_FRONTEND=noninteractive \
    HOME=/root \
    PATH=/root/.local/bin:/root/.claude/local/bin:/root/.opencode/bin:/usr/local/bin:/usr/bin:/bin

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl wget \
        git \
        jq \
        bash \
        python3 python3-pip \
        ripgrep fd-find \
 && rm -rf /var/lib/apt/lists/*

# Chromium for the chrome-devtools MCP server (Phase 7). The MCP
# launches it headless. Debian's chromium package is large (~400 MB)
# but is the simplest source; users who don't want it can pass
# AGENT_VM_NO_CHROME_MCP=1 to suppress the server entry in agent
# settings.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        chromium \
        fonts-liberation \
 && rm -rf /var/lib/apt/lists/* \
 && ln -sf /usr/bin/chromium /usr/bin/google-chrome \
 && ln -sf /usr/bin/chromium /usr/bin/google-chrome-stable \
 && mkdir -p /opt/google/chrome \
 && ln -sf /usr/bin/chromium /opt/google/chrome/chrome

# GitHub CLI from the official apt repo (Phase 6).
RUN install -dm 755 /etc/apt/keyrings \
 && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
        > /etc/apt/keyrings/githubcli-archive-keyring.gpg \
 && chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg \
 && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
        > /etc/apt/sources.list.d/github-cli.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends gh \
 && rm -rf /var/lib/apt/lists/*

# Node.js 22 from NodeSource — needed for Claude Code, OpenCode, MCP servers.
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
 && apt-get install -y --no-install-recommends nodejs \
 && rm -rf /var/lib/apt/lists/*

# Claude Code official installer.
RUN curl -fsSL https://claude.ai/install.sh | bash

# OpenCode official installer. Installs into ~/.opencode/bin; PATH already
# includes it. Symlink into /usr/local/bin so non-login shells find it too.
RUN curl -fsSL https://opencode.ai/install | bash \
 && ln -sf /root/.opencode/bin/opencode /usr/local/bin/opencode

# Codex CLI official installer.
RUN curl -fsSL https://github.com/openai/codex/releases/latest/download/install.sh | sh

# Sanity check at build time so a broken installer surfaces before we push.
RUN claude --version && opencode --version && codex --version

# Smoke entrypoint; the launcher overrides this in Phase 2.
CMD ["/bin/bash"]
