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
#
# `sudo` + `libnss3-tools` are here because the chrome MCP runs as a
# dedicated non-root `chrome` user (see the next RUN block). sudo is
# how the wrapper drops into it; libnss3-tools (`certutil`) is how the
# launcher injects the per-boot microsandbox CA into chrome's NSS DB.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        chromium \
        fonts-liberation \
        sudo \
        libnss3-tools \
 && rm -rf /var/lib/apt/lists/* \
 && ln -sf /usr/bin/chromium /usr/bin/google-chrome \
 && ln -sf /usr/bin/chromium /usr/bin/google-chrome-stable \
 && mkdir -p /opt/google/chrome \
 && ln -sf /usr/bin/chromium /opt/google/chrome/chrome

# Dedicated `chrome` user so chromium's user-namespace sandbox can
# initialize. As root chromium refuses to set up its sandbox and the
# CDP target closes immediately (`Protocol error
# (Target.setDiscoverTargets): Target closed`); the alternative is
# `--no-sandbox`, which we'd rather not pass — the microVM is the
# outer boundary, but keeping chromium's nested sandbox active is a
# real defence-in-depth gain when the agent makes the browser load
# untrusted content.
#
# /etc/passwd is edited by hand because debian-slim doesn't ship
# `useradd`. Empty NSS DB is created at image time so the per-launch
# `certutil -A` only has to add the CA, not also format the DB.
# Sudoers rule keeps `root -> chrome` password-less; no other
# (chrome -> root) transition is allowed.
#
# The wrapper script is what the in-image Chrome MCP entry actually
# launches: it re-execs `npx chrome-devtools-mcp@latest` under the
# `chrome` user with stdin/stdout preserved so the MCP JSON-RPC pipe
# from claude still works.
RUN echo 'chrome:x:9999:9999:Chrome MCP:/home/chrome:/bin/bash' >> /etc/passwd \
 && echo 'chrome:!:19000:0:99999:7:::' >> /etc/shadow \
 && echo 'chrome:x:9999:' >> /etc/group \
 && mkdir -p /home/chrome/.pki/nssdb \
 && certutil -d sql:/home/chrome/.pki/nssdb -N --empty-password \
 && chown -R 9999:9999 /home/chrome \
 && echo 'root ALL=(chrome) NOPASSWD: ALL' \
        > /etc/sudoers.d/agent-vm-chrome \
 && chmod 0440 /etc/sudoers.d/agent-vm-chrome \
 && printf '%s\n' \
        '#!/bin/bash' \
        '# agent-vm chrome MCP wrapper.' \
        '# Re-exec the MCP server under the `chrome` user so chromium'\''s' \
        '# user-namespace sandbox initializes (it refuses to as root).' \
        '# Args (npx + chrome-devtools-mcp + flags) come from the MCP' \
        '# config in ~/.claude.json; we just switch UID.' \
        '#' \
        '# We preserve a small allow-list of env vars across the sudo' \
        '# call: agentd boot-sets NODE_EXTRA_CA_CERTS / SSL_CERT_FILE /' \
        '# CURL_CA_BUNDLE / REQUESTS_CA_BUNDLE so any HTTPS client in' \
        '# the guest trusts the microsandbox MITM CA. Without these' \
        '# preserved, node fails `npx -y` with SELF_SIGNED_CERT_IN_CHAIN' \
        '# fetching from registry.npmjs.org and the MCP exits before' \
        '# the JSON-RPC handshake. PATH is needed so npx is findable.' \
        'set -e' \
        'exec sudo -u chrome -H -n \' \
        '    --preserve-env=NODE_EXTRA_CA_CERTS,SSL_CERT_FILE,CURL_CA_BUNDLE,REQUESTS_CA_BUNDLE,PATH \' \
        '    -- "$@"' \
        > /usr/local/bin/agent-vm-chrome-mcp \
 && chmod 0755 /usr/local/bin/agent-vm-chrome-mcp

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
