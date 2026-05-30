# agent-vm GUEST TEMPLATE image — the OCI image that the agent-vm
# tool boots *inside* its per-project microVM. NOT the agent-vm
# tool itself (that's the Rust binary published as
# @wirenboard/agent-vm on npm).
#
# Contents: Debian 13 slim + the three AI coding agents (Claude
# Code, OpenCode, Codex), Docker engine, network/diagnostic CLIs,
# codestyle-pinned Python linters (black/isort/pylint matching
# `wirenboard/codestyle`), and a clone of `wirenboard/codestyle`
# at /opt/wirenboard/codestyle. Two heavier blocks (WB
# build-essentials family + armhf/arm64 cross toolchains) are
# present in the Dockerfile but COMMENTED OUT for the initial
# roll-out — see the "DEFERRED" RUN blocks below. Other heavy
# extras (LSPs, additional MCP servers, docker-buildx) are
# deliberately deferred too.
#
# Layer policy: the file is ordered so the LAST RUN steps are the
# smallest and the most frequently invalidated. The hourly cron
# bumps the three agent installers (and the sanity check), and the
# `AGENT_INSTALL_CACHEBUST` ARG ensures Docker rebuilds those
# layers. Everything earlier — Debian base, chromium, docker
# engine, Python lint pins, codestyle clone — stays cached across
# hourly rebuilds, so users pulling `:latest` after a cron run
# only re-download ~tens of MB instead of the full image.
#
# Published to ghcr.io/wirenboard/agent-vm-template:latest by the
# hourly CI workflow (.github/workflows/build-image.yml). Source-
# checkout users can also build locally and push to a host-local
# registry via images/build.sh.

FROM debian:13-slim

LABEL org.opencontainers.image.title="agent-vm guest template"
LABEL org.opencontainers.image.description="Guest OCI image that the agent-vm tool (npm: @wirenboard/agent-vm) boots inside each per-project microVM. Contains Debian 13 + Claude Code + OpenCode + Codex + diag tooling (ssh/ping/etc.) + codestyle-pinned Python linters + /opt/wirenboard/codestyle. Rebuilt hourly to pick up new agent releases. NOT the agent-vm runtime itself — install that via `npm install -g @wirenboard/agent-vm`."

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
# and tooling additions (build-essential, ssh, etc.) do NOT change
# this — they're additive. See `crates/agent-vm/src/defaults.rs`
# for the matching range in the binary.
RUN echo 1 > /etc/agent-vm-image-version

# Conditionally trust a host-supplied CA bundle. Only fires when
# `images/build.sh` is invoked on a host that sits behind a TLS-
# intercept proxy (e.g. agent-vm-inside-agent-vm during local dev,
# or a corporate egress MITM). The build script passes the host CA
# via `--secret id=hostca` IF it exists at
# `/usr/local/share/ca-certificates/microsandbox-ca.crt`. In CI
# (no such file → no secret → empty mount), the `-s` test fails
# and this is a no-op — the production image is unchanged.
#
# `--mount=type=secret` keeps the CA file in the buildkit secret
# store, not in the layer; what IS persisted is the merged
# /etc/ssl/certs/ca-certificates.crt. That's fine: the launcher
# re-imports its per-boot CA at /usr/local/share/ca-certificates/
# at runtime anyway, so a stale build-time CA in the bundle is
# inert (and removed if a user runs update-ca-certificates).
#
# `CA_SHIM_CACHEBUST`: buildkit does NOT include secret content in
# the RUN cache key (by design — secrets shouldn't bake into
# images). So toggling secret presence across builds doesn't
# invalidate this layer on its own. `images/build.sh` passes the
# CA file's mtime (`stat -c %Y …`) when the secret is included, so
# any change to the CA file (or going from "secret absent" → "secret
# present") shifts the ARG and rebuilds. In CI (no secret, empty
# arg) the layer also no-ops and stays cached.
ARG CA_SHIM_CACHEBUST=
RUN --mount=type=secret,id=hostca,target=/tmp/hostca.crt,required=false \
    : "${CA_SHIM_CACHEBUST}" \
 && if [ -s /tmp/hostca.crt ]; then \
        apt-get update \
        && apt-get install -y --no-install-recommends ca-certificates \
        && install -d /usr/local/share/ca-certificates \
        && install -m 0644 /tmp/hostca.crt /usr/local/share/ca-certificates/agent-vm-build.crt \
        && update-ca-certificates \
        && rm -rf /var/lib/apt/lists/* \
        && echo "==> host CA trusted for this build"; \
    fi

# Base apt — interpreters, fetchers, search/text utilities, AND
# network/diagnostic CLIs. Bundling these into the existing base
# layer (instead of spinning up a new layer for "ssh + ping") keeps
# the layer count down: every additional layer costs a small
# per-layer materialize overhead in microsandbox even when nothing
# changes, so we only earn a new layer when we can articulate why
# it'd be invalidated independently.
#
# Network/diag tools rationale (engineers debug WB devices over
# SSH and serial; the in-VM agent often needs to confirm a host is
# reachable before chasing logs):
#   - iputils-ping, iputils-tracepath, traceroute, mtr-tiny:
#     reachability + per-hop latency.
#   - net-tools: legacy `ifconfig`, `netstat`, `route` — still
#     muscle-memory for most embedded engineers, and many WB docs
#     reference them by name.
#   - iproute2: modern `ip`, `ss`. Usually preinstalled, listed
#     explicitly so an upstream slimming doesn't silently drop it.
#   - openssh-client: `ssh`, `scp`, `sftp`, `ssh-keygen`. No server
#     — the agent should never accept inbound connections.
#   - sshpass: scripted password-auth SSH (lab fixtures, factory
#     test rigs); also used by some WB CI scripts.
#   - dnsutils: `dig`, `nslookup`, `host`.
#   - netcat-openbsd: `nc` for ad-hoc TCP/UDP poking.
#   - tcpdump: rare but invaluable when an HTTP MITM looks wrong.
#   - gnupg: needed below to add the GitHub CLI apt key (and any
#     user-side apt sources `.agent-vm.runtime.sh` adds).
#   - less, vim-tiny, file, unzip, zip, rsync, htop, lsof, strace,
#     tmux: standard "I'm in a shell, I need a tool" muscle-memory
#     set. Each is <5 MB; the bundle is well under 30 MB.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl wget \
        git \
        jq \
        bash \
        python3 python3-pip \
        ripgrep fd-find \
        gnupg \
        iputils-ping iputils-tracepath traceroute mtr-tiny \
        net-tools iproute2 \
        openssh-client sshpass \
        dnsutils netcat-openbsd tcpdump \
        less vim-tiny file unzip zip rsync \
        htop lsof strace tmux \
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

# Wiren Board C/C++ build essentials — DEFERRED.
#
# These two RUN blocks (build-essential family + armhf/arm64 cross
# toolchains) are commented out for the initial roll-out. They
# account for ~466 MiB compressed (~1.6 GB uncompressed) of the
# original image size estimate; deferring them keeps the first
# tooling-image bump lean while the layer ordering / cron-cache
# wins land. Uncomment to re-enable when WB engineers actually
# need the cross-compile path in-VM (the existing host
# `schroot -c bullseye-amd64-sbuild …` workflow still works).
#
# Sourced from the union of debian/control Build-Depends across
# wb-mqtt-serial, wb-mqtt-mbgate, wb-rules, wb-mqtt-confed and the
# `cpp/local/linux-devenv.sh` / `cpp/ci/clang-*-subdirs.sh` scripts
# in `wirenboard/codestyle`. When uncommenting, also re-add the
# corresponding sanity-check probes for clang-format,
# arm-linux-gnueabihf-gcc, aarch64-linux-gnu-gcc, and
# qemu-arm-static at the bottom of the Dockerfile.
#
#   - build-essential: gcc/g++/make/dpkg-dev metapackage.
#   - debhelper, devscripts, equivs, dpkg-cross: Debian packaging
#     primitives. `debhelper (>= 10)` is mandated by every WB
#     debian/control; `equivs` is required for `mk-build-deps -ir`
#     (the recommended `linux-devenv.sh` install path).
#   - pkg-config: standard build-time discovery.
#   - clang-format, clang-tidy: the only formatters/linters WB
#     codestyle ships configs for (`cpp/config/.clang-format`,
#     `.clang-tidy`). `cpp/ci/clang-format-subdirs.sh` and
#     `clang-tidy-subdirs.sh` call them directly.
#   - libcurl4-openssl-dev, libgtest-dev, libgmock-dev,
#     libmodbus-dev, libsystemd-dev: most-frequent C/C++ deps
#     across the WB services (modbus serial gateway, MQTT bridge,
#     etc).
#   - gcovr: pinned in `python/vscode/.devcontainer/devcontainer.json`
#     via the `coverage-gutters` extension; wb-mqtt-serial's
#     Build-Depends lists it explicitly.
#   - gdb-multiarch: install-listed by `cpp/local/linux-devenv.sh`;
#     debugging cross-built armhf/arm64 binaries.
#   - cmake, ninja-build: not in every WB repo but common enough
#     (firmware repo uses cmake) that excluding them would catch
#     people off guard.
#   - python3-cups, python3-libgpiod, python3-pycurl: per the
#     allowed-c-extensions list in `python/config/pyproject.toml`,
#     so pylint can resolve imports in WB Python projects without
#     spurious E0401.
# RUN apt-get update \
#  && apt-get install -y --no-install-recommends \
#         build-essential \
#         debhelper devscripts equivs dpkg-cross \
#         pkg-config \
#         clang-format clang-tidy \
#         libcurl4-openssl-dev \
#         libgtest-dev libgmock-dev \
#         libmodbus-dev \
#         libsystemd-dev \
#         gcovr \
#         gdb-multiarch \
#         cmake ninja-build \
#         python3-cups python3-libgpiod python3-pycurl \
#  && rm -rf /var/lib/apt/lists/*

# Wiren Board cross-build toolchains — armhf + arm64 — DEFERRED.
#
# See the deferment note on the build-essentials block above.
#
# Every flagship WB binary ships for one of these two architectures
# (controllers are armhf, newer hardware is arm64); the canonical
# build command in `codestyle/cpp/vscode/.vscode/tasks.json` is
# `schroot -c bullseye-amd64-sbuild -- DEB_HOST_MULTIARCH=arm-…
# make -j12`, and qemu-user-static is what makes the resulting
# binary runnable on x86_64 for tests.
#
# Kept in a SEPARATE layer from the C/C++ essentials above because
# it's the single biggest discretionary addition (~300 MB
# uncompressed / ~258 MiB compressed). A future Dockerfile.slim
# can keep this commented while uncommenting the build-essentials
# block above.
#
#   - crossbuild-essential-armhf, crossbuild-essential-arm64:
#     gcc-arm-linux-gnueabihf / gcc-aarch64-linux-gnu meta-packages.
#   - qemu-user-static: registers binfmt handlers so an `armhf` or
#     `arm64` ELF executes under qemu transparently. Required by
#     the `[armhf]/[arm64] Run tests for debug` tasks in codestyle.
#   - debootstrap, schroot, sbuild: the canonical WB build path is
#     `sbuild` inside a `schroot` named `bullseye-amd64-sbuild`.
#     The chroot itself is NOT created here (it's per-host, lives
#     at /srv/chroot, and requires the schroot config; the user
#     creates it on first need with `sbuild-createchroot`).
# RUN apt-get update \
#  && apt-get install -y --no-install-recommends \
#         crossbuild-essential-armhf \
#         crossbuild-essential-arm64 \
#         qemu-user-static \
#         debootstrap schroot sbuild \
#  && rm -rf /var/lib/apt/lists/*

# Python lint/format toolchain — pinned to the versions in
# `wirenboard/codestyle/python/config/requirements.txt`. Pinning
# matters: codestyle ships a `pyproject.toml` that engineers copy
# into projects, and a black/isort/pylint version skew between
# image and project re-formats files differently → noisy diffs.
#
# `--break-system-packages` because Debian 13 enforces PEP 668 and
# we WANT these system-wide so any in-VM tool finds them. The
# microVM rootfs is throwaway, so there's no system Python we can
# accidentally damage. `--no-cache-dir` shaves ~40 MB off the
# layer (the pip download cache is dead weight in an image).
#
# Versions sourced from
# `codestyle/python/config/requirements.txt` (master). `attrs==23.1.0`
# is per the workaround in `codestyle/python/local/linux-devenv.sh`
# (newer attrs breaks the pinned pylint). `j2cli` is in
# wb-mqtt-serial's Build-Depends and is a one-line install here.
RUN pip3 install --no-cache-dir --break-system-packages \
        black==24.2.0 \
        isort==5.13.2 \
        pylint==3.3.9 \
        pytest-cov==2.10.1 \
        attrs==23.1.0 \
        j2cli

# Shallow-clone `wirenboard/codestyle` so the per-project
# devenv scripts work without each project re-cloning it.
#
# The script `cpp/local/linux-devenv.sh` and its Python sibling
# both expect to be invoked from inside a project as
# `bash ../codestyle/{cpp,python}/local/linux-devenv.sh`, i.e.
# with codestyle as a sibling directory. Having it at a known
# path inside the image lets a project hook (`.agent-vm.runtime.sh`)
# symlink/copy it into place at launch instead of cloning over
# the proxy on every cold start.
#
# `CODESTYLE_REF` build-arg (default `master`) lets a reproducible
# build pin to a specific ref — `git clone --branch` accepts a
# branch or tag name. The hourly CI cron just builds from master,
# matching the previous behavior.
#
# Hard-fails by default: if GitHub is unreachable the resulting
# image is missing the devenv path that several WB workflows rely
# on, and we'd rather not silently ship that. Source-checkout dev
# builds that set `AGENT_INSTALL_SOFT_FAIL=1` (see images/build.sh,
# auto-set on TLS-intercept hosts) downgrade to a warning.
#
# `AGENT_INSTALL_SOFT_FAIL` is also consumed by the three agent
# installers + the sanity check below; declared here at first use
# so a single toggle covers everything that might legitimately fail
# in a dev sandbox but must succeed in CI.
ARG CODESTYLE_REF=master
ARG AGENT_INSTALL_SOFT_FAIL=
RUN git clone --depth=1 --branch "${CODESTYLE_REF}" \
        https://github.com/wirenboard/codestyle.git \
        /opt/wirenboard/codestyle \
 || { msg="==> wirenboard/codestyle clone FAILED (ref=${CODESTYLE_REF}) — re-clone in-VM with: git clone https://github.com/wirenboard/codestyle.git /opt/wirenboard/codestyle"; \
      if [ -n "${AGENT_INSTALL_SOFT_FAIL:-}" ]; then echo "$msg (soft-fail mode)"; else echo "$msg" >&2; exit 1; fi; }

# Cache-bust for the three agent installers below. The workflow
# passes the hourly timestamp tag as the value so each scheduled
# build invalidates exactly these RUN layers (and the sanity check
# that follows) while leaving every earlier layer — Debian base,
# chromium, docker engine, WB build deps, cross toolchains, Python
# lint pins, codestyle clone — cached. Without this the GHA layer
# cache reuses the `curl … install.sh | bash` layers indefinitely,
# so hourly cron runs never picked up new claude-code/codex/
# opencode releases. Local `images/build.sh` builds leave the
# default empty value and get the normal Docker layer-cache
# behaviour.
ARG AGENT_INSTALL_CACHEBUST=

# Shared helper for the three agent installer RUNs below. Downloads
# the upstream `install.sh`, runs it under the requested shell, and
# emits a uniform soft-fail or hard-fail diagnostic depending on
# `AGENT_INSTALL_SOFT_FAIL`. Keeping the policy in one place means
# any future tweak (extra retries, log format, soft-fail discrimina-
# tion) lives in exactly one spot — the three RUNs that call it are
# trivial one-liners.
#
# Why curl-only (no python3-urllib fallback): both curl and python3's
# `ssl` module link against the same system OpenSSL, so a real TLS-
# layer failure (the curl 56 "SSL_read: unexpected eof" we see from
# inside MITM-proxied dev sandboxes) hits the fallback identically.
# `--retry 5 --retry-all-errors --http1.1` covers the transient cases
# that ARE recoverable. The MITM-sandbox case is what
# `AGENT_INSTALL_SOFT_FAIL=1` exists for.
#
# Written via `printf '%s\n'` because debian-slim's `/bin/sh` is
# POSIX dash, no heredoc niceties. The script is small enough that
# adding a layer for it is essentially free (well under 1 KB).
RUN printf '%s\n' \
        '#!/bin/sh' \
        '# agent-vm-install <name> <shell> <url>' \
        '#   <name>  - agent identifier (codex, opencode, claude)' \
        '#   <shell> - sh|bash; the interpreter used to run the installer' \
        '#   <url>   - upstream installer URL' \
        '# Honors $AGENT_INSTALL_SOFT_FAIL: when non-empty, a download or' \
        '# install failure becomes a warning + exit 0 instead of exit 1.' \
        'set -eu' \
        'name=$1; shell=$2; url=$3' \
        'tmp=$(mktemp)' \
        'trap "rm -f \"$tmp\"" EXIT INT TERM' \
        'if curl -fsSL --retry 5 --retry-all-errors --http1.1 "$url" -o "$tmp" \' \
        '        && "$shell" "$tmp"; then' \
        '    exit 0' \
        'fi' \
        'msg="==> $name installer FAILED — install in-VM with: curl -fsSL $url | $shell"' \
        'if [ -n "${AGENT_INSTALL_SOFT_FAIL:-}" ]; then' \
        '    echo "$msg (soft-fail mode; image will ship without $name)"' \
        '    exit 0' \
        'fi' \
        'echo "$msg" >&2' \
        'exit 1' \
        > /usr/local/bin/agent-vm-install \
 && chmod 0755 /usr/local/bin/agent-vm-install \
 && test -s /usr/local/bin/agent-vm-install

# The three agents each get their own RUN so a single agent
# release only invalidates one layer (~30 MB per agent). Order is
# rough size order (smallest first) so the steady-state worst-
# case download — a fresh release for Claude Code (the biggest)
# — only re-pulls the topmost layer.
#
# Agent installers HARD-FAIL by default — if an upstream installer
# is broken or removed, the hourly cron should NOT silently ship an
# image missing an agent. `AGENT_INSTALL_SOFT_FAIL=1` (build-arg,
# auto-set by images/build.sh on TLS-intercept dev hosts; CI never
# sets it) turns the three RUNs into warnings instead.

# Codex CLI official installer.
RUN agent-vm-install codex sh \
        https://github.com/openai/codex/releases/latest/download/install.sh

# OpenCode official installer. Installs into ~/.opencode/bin; PATH already
# includes it. Symlink into /usr/local/bin so non-login shells find it too.
# The symlink is best-effort — under soft-fail mode the install may have
# been skipped, in which case there's nothing to link.
RUN agent-vm-install opencode bash https://opencode.ai/install \
 && { [ -x /root/.opencode/bin/opencode ] \
        && ln -sf /root/.opencode/bin/opencode /usr/local/bin/opencode \
        || true; }

# Claude Code official installer.
RUN agent-vm-install claude bash https://claude.ai/install.sh

# Sanity check at build time. Every command is invoked directly
# (`tool --version >/dev/null`) so a present-but-broken binary —
# corrupt download, broken symlink target, missing libc symbol —
# fails the build instead of slipping through a `command -v` exists-
# check. `set -e` propagates the first failure.
#
# Agent CLIs (claude/opencode/codex) are HARD-required unless
# `AGENT_INSTALL_SOFT_FAIL` is set, in which case they're SOFT
# (missing → warning, not failure) — matches the policy on the
# installer RUNs above. /opt/wirenboard/codestyle follows the same
# policy (its clone RUN above also honors AGENT_INSTALL_SOFT_FAIL).
RUN set -e \
 && echo "== HARD requirements ==" \
 && dockerd --version >/dev/null \
 && docker --version >/dev/null \
 && containerd --version >/dev/null \
 && runc --version >/dev/null \
 && black --version >/dev/null \
 && isort --version >/dev/null \
 && pylint --version >/dev/null \
 && ssh -V >/dev/null 2>&1 \
 && sshpass -V >/dev/null 2>&1 \
 && ping -V >/dev/null 2>&1 \
 && { if [ -d /opt/wirenboard/codestyle ]; then \
          echo "  codestyle clone: OK"; \
      elif [ -n "${AGENT_INSTALL_SOFT_FAIL:-}" ]; then \
          echo "  codestyle clone: MISSING (soft-fail mode)"; \
      else \
          echo "  codestyle clone: MISSING — hard sanity failure" >&2; exit 1; \
      fi; } \
 && echo "== agent CLIs ==" \
 && for bin in claude opencode codex; do \
        if "$bin" --version >/dev/null 2>&1; then \
            ver=$("$bin" --version 2>&1 | head -1); \
            echo "  $bin: $ver"; \
        elif [ -n "${AGENT_INSTALL_SOFT_FAIL:-}" ]; then \
            echo "  $bin: MISSING (soft-fail mode)"; \
        else \
            echo "  $bin: MISSING — hard sanity failure" >&2; \
            exit 1; \
        fi; \
    done \
 && echo "== sanity OK =="

# Smoke entrypoint; the launcher overrides this in Phase 2.
CMD ["/bin/bash"]
