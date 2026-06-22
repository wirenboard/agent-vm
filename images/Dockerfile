# agent-vm GUEST TEMPLATE image — the OCI image that the agent-vm
# tool boots *inside* its per-project microVM. NOT the agent-vm
# tool itself (that's the Rust binary published as
# @wirenboard/agent-vm on npm).
#
# Contents: Debian 13 slim + the four AI coding agents (Claude
# Code, OpenCode, Codex, GitHub Copilot), minimum dev tooling, and the docker engine
# (docker.io + containerd + runc + fuse-overlayfs). Other heavy
# extras (LSPs, additional MCP servers, docker-buildx) are
# deliberately deferred.
#
# Published to ghcr.io/wirenboard/agent-vm-template:latest by the
# hourly CI workflow (.github/workflows/build-image.yml). Source-
# checkout users can also build locally and push to a host-local
# registry via images/build.sh.

FROM debian:13-slim

LABEL org.opencontainers.image.title="agent-vm guest template"
LABEL org.opencontainers.image.description="Guest OCI image that the agent-vm tool (npm: @wirenboard/agent-vm) boots inside each per-project microVM. Contains Debian 13 + Claude Code + OpenCode + Codex + GitHub Copilot + minimal dev tooling. Rebuilt hourly to pick up new agent releases. NOT the agent-vm runtime itself — install that via `npm install -g @wirenboard/agent-vm`."

# LANG=C.UTF-8: debian:slim ships with NO locale set, leaving the guest in
# C/POSIX — which renders a non-ASCII (e.g. Cyrillic) cwd as `M-P…` meta-
# escapes in bash/readline AND makes locale-driven filesystem encodings
# default to ASCII, so Python/Node/ripgrep mis-handle or error on a non-ASCII
# path even when it mounted fine. C.UTF-8 is always present in glibc (no
# locale-gen needed), ASCII-sorting + English-messages but UTF-8-aware.
# agent-vm's launcher also sets this for every exec; having it in the image
# makes any other use of the image correct too.
ENV DEBIAN_FRONTEND=noninteractive \
    HOME=/root \
    PATH=/root/.local/bin:/root/.claude/local/bin:/root/.opencode/bin:/usr/local/bin:/usr/bin:/usr/sbin:/bin \
    LANG=C.UTF-8

# Image-API contract version. Bump on BREAKING image changes only
# (new required mount points, changed env-var contracts, removed
# binaries, renamed in-VM paths). Routine agent-version refreshes
# do NOT change this. See `crates/agent-vm/src/defaults.rs` for the
# matching range in the binary.
RUN echo 1 > /etc/agent-vm-image-version

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
        '# Preserve list (passed through sudo'\''s env_reset):' \
        '#  - NODE_EXTRA_CA_CERTS / SSL_CERT_FILE / CURL_CA_BUNDLE /' \
        '#    REQUESTS_CA_BUNDLE: agentd boot-sets these so any HTTPS' \
        '#    client trusts the microsandbox MITM CA. Without them' \
        '#    node fails `npx -y` with SELF_SIGNED_CERT_IN_CHAIN.' \
        '#  - PATH: needed so npx is findable (sudo'\''s secure_path' \
        '#    would otherwise replace it).' \
        '#  - HTTP_PROXY / HTTPS_PROXY / NO_PROXY (upper/lowercase):' \
        '#    NOT set by agentd (microsandbox uses transparent network' \
        '#    interception, no HTTP proxy env). Forwarded so that a' \
        '#    user-supplied MCP `env:` block or .agent-vm.runtime.sh' \
        '#    setting these is honoured by chromium.' \
        '#  - CHROME_DEVTOOLS_MCP_NO_USAGE_STATISTICS / CI / DEBUG:' \
        '#    user/MCP-config opt-outs and debugging knobs that' \
        '#    chrome-devtools-mcp documents.' \
        '#  - TZ / LANG / LC_ALL / TMPDIR: locale + sane temp paths.' \
        '#' \
        '# NOT in the preserve list (deliberate):' \
        '#  - USER / LOGNAME: sudo'\''s default `env_reset` policy re-' \
        '#    derives these from the target passwd entry. Adding them' \
        '#    here would forward root'\''s USER into the chrome session.' \
        '#  - HOME: set by `-H` from chrome'\''s passwd entry (not by' \
        '#    env passthrough).' \
        '#' \
        '# `cd /home/chrome` keeps chromium'\''s default CWD writable' \
        '# (the agent runs as root in /workspace, where chrome (UID' \
        '# 9999) cannot write; tools that emit relative paths' \
        '# (./trace.json, ./screenshot.png) would otherwise EACCES).' \
        '# Diagnostic on failure so Claude doesn'\''t just see the MCP' \
        '# transport close before handshake with no breadcrumb.' \
        'set -e' \
        'cd /home/chrome 2>/dev/null || {' \
        '    echo "agent-vm-chrome-mcp: cannot cd to /home/chrome -- image misconfigured?" >&2' \
        '    exit 1' \
        '}' \
        '# Import the microsandbox MITM CA into the chrome-owned NSS DB so' \
        '# chromium trusts the intercept proxy. chromium on Linux ignores' \
        '# the system CA bundle (which curl/node use), honouring only its' \
        '# built-in roots + this per-user NSS DB. Done here, once, at MCP' \
        '# startup -- NOT on every agent-vm launch (previously a ~270ms' \
        '# certutil fork in the launcher prelude) -- and skipped when' \
        '# chrome is unused (the MCP server is only spawned when enabled).' \
        '# The DB is chrome-owned (UID 9999), so drop to chrome to write it.' \
        '# Non-fatal: a failure should warn, not kill the MCP.' \
        '_ca=/usr/local/share/ca-certificates/microsandbox-ca.crt' \
        'if [ -f "$_ca" ]; then' \
        '    sudo -u chrome -n -- certutil -d sql:/home/chrome/.pki/nssdb -A \' \
        '        -t C,, -n microsandbox -i "$_ca" \' \
        '        || echo "agent-vm-chrome-mcp: warning: failed to import microsandbox CA (HTTPS may fail ERR_CERT_AUTHORITY_INVALID)" >&2' \
        'fi' \
        'exec sudo -u chrome -H -n \' \
        '    --preserve-env=NODE_EXTRA_CA_CERTS,SSL_CERT_FILE,CURL_CA_BUNDLE,REQUESTS_CA_BUNDLE \' \
        '    --preserve-env=PATH,HTTP_PROXY,HTTPS_PROXY,NO_PROXY,http_proxy,https_proxy,no_proxy \' \
        '    --preserve-env=CHROME_DEVTOOLS_MCP_NO_USAGE_STATISTICS,CI,DEBUG \' \
        '    --preserve-env=TZ,LANG,LC_ALL,TMPDIR \' \
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

# Pre-warm chrome-devtools-mcp's npm cache as the chrome user so the
# first browser tool call in any sandbox doesn't pay the multi-second
# `npx -y` fetch cost. `--help` exits cleanly after install/version-
# check; it's the cheapest exercise of the full install path.
#
# Pinned to a known-good version. The runtime MCP config in
# `crates/agent-vm/src/secrets.rs::write_default_claude_root_state`
# pins the SAME version — bump both together. Without the pin the
# cache stops being useful the first time upstream cuts a release:
# `npx -y` re-resolves `@latest` against the registry and re-
# downloads under the writable upper layer.
#
# Best-effort: `|| true` because npm-registry transient failure or a
# bad `--help` exit code shouldn't fail the whole `agent-vm setup`.
# A missing cache costs a multi-second fetch on first browser tool
# call; failing the image build is much worse.
#
# Must come AFTER the nodejs RUN above — npx didn't exist before it.
RUN sudo -u chrome -H npx -y chrome-devtools-mcp@1.0.1 --help >/dev/null 2>&1 \
 || echo "==> pre-warm skipped (will lazy-fetch chrome-devtools-mcp at first MCP call)"

# Docker engine — dockerd + cli + containerd + runc.
#
# Why each piece:
#   - docker.io / docker-cli: the daemon and the CLI a user actually
#     invokes; no buildx (legacy builder is fine for the small builds
#     agents do in-VM; +20 MB compressed isn't worth the polish).
#   - containerd / runc: the OCI runtime stack the daemon delegates to.
#     Both are kernel-required (runc=container init, containerd=image
#     management); without them dockerd refuses to start.
#   - fuse-overlayfs: the storage driver we pin in daemon.json below.
#     Native overlay2 doesn't work because the VM rootfs is already
#     overlay (libkrun rootfs design), and overlay-on-overlay returns
#     EINVAL inside dockerd's snapshotter. fuse-overlayfs sidesteps
#     that at userspace level.
#   - iptables: dockerd installs NAT rules on every container start;
#     missing binary makes the daemon log a warning and disable bridge
#     networking. The kernel-side iptables/nf_tables support comes
#     from the libkrunfw-overrides patch (see libkrunfw-overrides/
#     README.md).
#
# Storage-driver choice (daemon.json): see comment above. iptables
# alternative left at debian's default (iptables-nft) — both nft and
# legacy backends are functional in the patched kernel.
#
# Auto-start: not done in the image. The launcher (or a per-project
# `.agent-vm.runtime.sh`) is the right place to `dockerd &` if the
# session needs it — most sessions won't, so paying ~50 MB RSS at
# every boot would be wasteful.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        docker.io docker-cli \
        containerd runc \
        fuse-overlayfs \
        iptables \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /etc/docker \
 && printf '%s\n' '{' '  "storage-driver": "fuse-overlayfs"' '}' \
        > /etc/docker/daemon.json

# Cache-bust for the three agent installers below. The workflow
# passes the hourly timestamp tag as the value so each scheduled
# build invalidates exactly these RUN layers (and the sanity check
# that follows) while leaving the heavy apt/chromium/docker/node
# layers cached. Without this the GHA layer cache reuses the
# `curl … install.sh | bash` layers indefinitely, so hourly cron
# runs never picked up new claude-code/codex/opencode releases.
# Local `images/build.sh` builds leave the default empty value and
# get the normal Docker layer-cache behaviour.
ARG AGENT_INSTALL_CACHEBUST=

# Claude Code official installer.
RUN curl -fsSL https://claude.ai/install.sh | bash

# Claude Code LSP plugins (C/C++, Python, TypeScript, Go) baked in so
# in-VM Claude has code intelligence on first launch. Ports the four
# language servers the original `setup` installed (claude-vm.sh:348-362):
# clangd-lsp / pyright-lsp / typescript-lsp / gopls-lsp from the
# claude-plugins-official marketplace.
#
# Marketplace registration first: `claude plugin marketplace add` clones
# https://github.com/anthropics/claude-plugins-official.git into Claude's
# plugin state, then each `claude plugin install <name>@<marketplace>`
# resolves against it. The bare repo URL is what the original git-cloned
# by hand; `marketplace add` lets the CLI manage the clone + index for us.
#
# Best-effort per the chrome-mcp pre-warm style above: a transient
# registry/clone blip during the hourly build must not fail the whole
# image. Each step is guarded with `|| echo skipped`; a missing plugin
# costs in-VM code intelligence for that language, not a broken image —
# Claude still boots and the user can re-run `claude plugin install`.
#
# Must come AFTER the Claude install above — `claude` didn't exist before
# it. node/npm (from the nodejs RUN) and git (from the base apt RUN) are
# both already present, which the marketplace clone + plugin resolution
# need.
#
# Invoke claude by ABSOLUTE PATH (/root/.local/bin/claude): the installer
# drops the binary there but the only ENV PATH in this file is
# /usr/local/cargo/bin:$PATH, and Docker RUN uses a non-login /bin/sh that
# never sources the shell profile the installer edits. A bare `claude`
# would hit command-not-found (rc 127), every `|| echo skipped` guard would
# fire, and the RUN would still exit 0 — baking a GREEN image with ZERO LSP
# plugins. The absolute path matches the `--version` sanity check below and
# the OpenCode block's /root/.opencode/bin/opencode.
#
# A `claude plugin list` runs after the loop so the build log shows which
# plugins actually landed — without it the best-effort guards would hide a
# total no-op behind a clean build. If `marketplace add` ever rejects the
# `owner/repo` shorthand, swap in the explicit URL
# https://github.com/anthropics/claude-plugins-official.git.
RUN { /root/.local/bin/claude plugin marketplace add anthropics/claude-plugins-official \
        || echo "==> LSP marketplace add skipped (will lazy-add at first plugin use)"; } \
 && for lsp in clangd-lsp pyright-lsp typescript-lsp gopls-lsp; do \
        /root/.local/bin/claude plugin install "${lsp}@claude-plugins-official" \
        || echo "==> LSP plugin ${lsp} skipped (install at runtime if needed)"; \
    done \
 && echo "==> LSP plugins installed at build time:" \
 && { /root/.local/bin/claude plugin list || true; }

# OpenCode official installer. Installs into ~/.opencode/bin; PATH already
# includes it. Symlink into /usr/local/bin so non-login shells find it too.
RUN curl -fsSL https://opencode.ai/install | bash \
 && ln -sf /root/.opencode/bin/opencode /usr/local/bin/opencode

# Codex CLI official installer.
RUN curl -fsSL https://github.com/openai/codex/releases/latest/download/install.sh | sh

# D1: GitHub Copilot CLI. Its own RUN layer so the AGENT_INSTALL_CACHEBUST
# arg above invalidates it on the hourly build like the other agents, and
# so a wrong package/binary name fails the image build loudly (via the
# `copilot --version` smoke check) instead of surfacing as a
# command-not-found inside the guest at `agent-vm copilot` time.
RUN npm install -g @github/copilot && copilot --version

# Sanity check at build time so a broken installer surfaces before we push.
RUN claude --version && opencode --version && codex --version && copilot --version \
 && dockerd --version && docker --version && containerd --version && runc --version

# Smoke entrypoint; the launcher overrides this in Phase 2.
CMD ["/bin/bash"]
