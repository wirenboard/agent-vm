#!/usr/bin/env bash
#
# agent-vm: Run AI coding agents inside a sandboxed Lima VM
# Part of https://github.com/sylvinus/agent-vm
#
# Source this file in your shell config:
#   source /path/to/agent-vm/claude-vm.sh
#
# Usage:
#   agent-vm setup            - Create the VM template (run once)
#   agent-vm claude [args]    - Run Claude in a fresh VM (args forwarded to claude)
#   agent-vm opencode [args]  - Run OpenCode in a fresh VM (args forwarded to opencode)
#   agent-vm shell [agent]    - Open a debug shell in a fresh VM

CLAUDE_VM_TEMPLATE="claude-template"

_agent_vm_state_root() {
  if [ -n "${AGENT_VM_STATE_DIR:-}" ]; then
    printf '%s\n' "$AGENT_VM_STATE_DIR"
  else
    printf '%s\n' "${XDG_STATE_HOME:-$HOME/.local/state}/agent-vm"
  fi
}

_agent_vm_project_hash() {
  local host_dir="$1"
  printf '%s' "$host_dir" | shasum -a 256 | awk '{print $1}'
}

_agent_vm_project_state_dir() {
  local host_dir="$1"
  local root
  local hash
  local slug
  local state_dir

  root="$(_agent_vm_state_root)"
  hash="$(_agent_vm_project_hash "$host_dir")"
  slug="$(basename "$host_dir" | tr -cs 'a-zA-Z0-9' '-' | sed 's/^-//;s/-$//')"
  [ -z "$slug" ] && slug="project"

  state_dir="${root}/${slug}-${hash:0:12}"
  mkdir -p "$state_dir"
  printf '%s\n' "$state_dir"
}

# ── USB passthrough helpers ───────────────────────────────────────────────────

_agent_vm_usb_resolve() {
  local device="$1"
  # Already in VID:PID format
  if [[ "$device" =~ ^[0-9a-fA-F]{4}:[0-9a-fA-F]{4}$ ]]; then
    printf '%s\n' "$device"
    return 0
  fi
  # Device path like /dev/ttyACM0
  if [[ "$device" =~ ^/dev/ ]]; then
    local devname="${device#/dev/}"
    local sysdir="/sys/class/tty/${devname}/device"
    if [ ! -d "$sysdir" ]; then
      # Try other device classes (hidraw, usb, etc.)
      local class="${devname%%[0-9]*}"
      sysdir="/sys/class/${class}/${devname}/device"
    fi
    if [ ! -d "$sysdir" ]; then
      echo "Error: Cannot find sysfs entry for $device" >&2
      return 1
    fi
    # Walk up to the USB device level (has idVendor)
    # Resolve symlinks first so dirname walks the real device tree
    local usbdir
    usbdir="$(readlink -f "$sysdir")"
    while [ -n "$usbdir" ] && [ "$usbdir" != "/" ] && [ ! -f "$usbdir/idVendor" ]; do
      usbdir="$(dirname "$usbdir")"
    done
    if [ ! -f "$usbdir/idVendor" ]; then
      echo "Error: Cannot find USB vendor/product ID for $device" >&2
      return 1
    fi
    local vid pid
    vid="$(cat "$usbdir/idVendor")"
    pid="$(cat "$usbdir/idProduct")"
    printf '%s:%s\n' "$vid" "$pid"
    return 0
  fi
  echo "Error: Unrecognized device format '$device' (use /dev/ttyACM0 or 1a86:55d3)" >&2
  return 1
}

_agent_vm_usb_find_sysfs() {
  local vidpid="$1"
  local vid="${vidpid%%:*}"
  local pid="${vidpid##*:}"
  local devpath
  for devpath in /sys/bus/usb/devices/[0-9]*-[0-9]*; do
    [ -f "$devpath/idVendor" ] || continue
    [ "$(cat "$devpath/idVendor")" = "$vid" ] || continue
    [ "$(cat "$devpath/idProduct")" = "$pid" ] || continue
    printf '%s\n' "$(basename "$devpath")"
    return 0
  done
  echo "Error: No USB device found matching $vidpid" >&2
  return 1
}

_agent_vm_usb_unbind() {
  local port="$1"
  # Unbind each interface from its current driver
  local intf
  for intf in /sys/bus/usb/devices/${port}:*/driver; do
    [ -d "$intf" ] || continue
    local intf_name
    intf_name="$(basename "$(dirname "$intf")")"
    echo "  Unbinding $intf_name from $(basename "$(readlink "$intf")")"
    sudo sh -c "echo '$intf_name' > '$intf/unbind'"
  done
  # chmod the USB device node so QEMU can access it without root
  local busnum devnum devnode
  busnum="$(cat "/sys/bus/usb/devices/${port}/busnum")"
  devnum="$(cat "/sys/bus/usb/devices/${port}/devnum")"
  devnode=$(printf '/dev/bus/usb/%03d/%03d' "$busnum" "$devnum")
  echo "  Setting permissions on $devnode"
  sudo chmod 0666 "$devnode"
}

_agent_vm_usb_rebind() {
  local port="$1"
  echo "Rebinding USB device $port..."
  sudo sh -c "echo '$port' > /sys/bus/usb/drivers/usb/unbind" 2>/dev/null
  sudo sh -c "echo '$port' > /sys/bus/usb/drivers/usb/bind" 2>/dev/null
}

_agent_vm_qemu_wrapper() {
  # Create a QEMU wrapper with optional virtio-balloon and USB passthrough.
  # Usage: _agent_vm_qemu_wrapper [--no-balloon] [usb_vid:pid ...]
  local use_balloon=true
  if [ "${1:-}" = "--no-balloon" ]; then
    use_balloon=false
    shift
  fi
  local wrapper
  wrapper="$(mktemp /tmp/qemu-wrapper.XXXXXX)"
  local qemu_bin
  qemu_bin="$(command -v qemu-system-x86_64)"
  local extra_args=""
  if $use_balloon; then
    # Pin balloon to a high PCI address so it doesn't shift the boot disk
    # (the UEFI NVRAM from the template has hard-coded PCI paths for boot entries)
    extra_args="-device virtio-balloon-pci,id=balloon0,deflate-on-oom=on,addr=0x18"
  fi
  if [ $# -gt 0 ]; then
    extra_args+="${extra_args:+ }-device qemu-xhci,id=xhci,addr=0x15"
    local vidpid
    for vidpid in "$@"; do
      local vid="${vidpid%%:*}"
      local pid="${vidpid##*:}"
      extra_args+=" -device usb-host,bus=xhci.0,vendorid=0x${vid},productid=0x${pid}"
    done
  fi
  cat > "$wrapper" << WRAPPER
#!/bin/sh
exec "$qemu_bin" "\$@" $extra_args
WRAPPER
  chmod +x "$wrapper"
  printf '%s\n' "$wrapper"
}

_agent_vm_setup() {
  local minimal=false
  local disk=30
  local _has_balloon=false
  command -v qemu-system-x86_64 &>/dev/null && _has_balloon=true
  local memory=""
  if $_has_balloon; then
    memory=16  # balloon ceiling: guest starts small, grows on demand
  else
    memory=4   # no balloon (macOS VZ): static allocation
  fi

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --help|-h)
        echo "Usage: agent-vm setup [--minimal] [--disk GB] [--memory GB]"
        echo ""
        echo "Create a VM template with Claude Code and OpenCode pre-installed."
        echo ""
        echo "Options:"
        echo "  --minimal      Only install git, curl, jq, Claude Code, and OpenCode"
        echo "  --disk GB      VM disk size (default: 30)"
        echo "  --memory GB    VM memory (default: 16 on Linux with balloon, 4 on macOS)"
        echo "  --help         Show this help"
        return 0
        ;;
      --minimal)
        minimal=true
        shift
        ;;
      --disk)
        disk="$2"
        shift 2
        ;;
      --disk=*)
        disk="${1#*=}"
        shift
        ;;
      --memory)
        memory="$2"
        shift 2
        ;;
      --memory=*)
        memory="${1#*=}"
        shift
        ;;
      *)
        echo "Unknown option: $1" >&2
        echo "Usage: agent-vm setup [--minimal] [--disk GB] [--memory GB]" >&2
        return 1
        ;;
    esac
  done

  if ! command -v limactl &>/dev/null; then
    if command -v brew &>/dev/null; then
      echo "Installing Lima..."
      brew install lima
    else
      echo "Error: Lima is required. Install from https://lima-vm.io/docs/installation/" >&2
      return 1
    fi
  fi

  limactl stop "$CLAUDE_VM_TEMPLATE" &>/dev/null
  limactl delete "$CLAUDE_VM_TEMPLATE" --force &>/dev/null

  echo "Creating VM template..."
  limactl create --name="$CLAUDE_VM_TEMPLATE" template:debian-13 \
    --set '.mounts=[]' \
    --set '.containerd.system=false' \
    --set '.containerd.user=false' \
    --disk="$disk" \
    --memory="$memory" \
    --tty=false
  limactl start "$CLAUDE_VM_TEMPLATE"

  # Disable needrestart's interactive prompts
  limactl shell "$CLAUDE_VM_TEMPLATE" sudo bash -c 'mkdir -p /etc/needrestart/conf.d && echo "\$nrconf{restart} = '"'"'a'"'"';" > /etc/needrestart/conf.d/no-prompt.conf'

  echo "Installing base packages..."
  limactl shell "$CLAUDE_VM_TEMPLATE" sudo DEBIAN_FRONTEND=noninteractive apt-get update
  # sshfs is required for Lima's reverse-sshfs mounts; pre-installing it avoids
  # a slow apt-get update + install on every clone boot (~15s savings)
  limactl shell "$CLAUDE_VM_TEMPLATE" sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
    git curl jq sshfs

  if ! $minimal; then
    limactl shell "$CLAUDE_VM_TEMPLATE" sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
      wget build-essential \
      python3 python3-pip python3-venv \
      ripgrep fd-find htop \
      unzip zip \
      ca-certificates \
      qemu-user-static binfmt-support \
      mosquitto-clients \
      gh

    # Install Docker from official repo (includes docker compose)
    echo "Installing Docker..."
    limactl shell "$CLAUDE_VM_TEMPLATE" bash -c '
      sudo install -m 0755 -d /etc/apt/keyrings
      sudo curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
      sudo chmod a+r /etc/apt/keyrings/docker.asc
      echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian $(. /etc/os-release && echo "$VERSION_CODENAME") stable" | sudo tee /etc/apt/sources.list.d/docker.list > /dev/null
    '
    limactl shell "$CLAUDE_VM_TEMPLATE" sudo DEBIAN_FRONTEND=noninteractive apt-get update
    limactl shell "$CLAUDE_VM_TEMPLATE" sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
      docker-ce docker-ce-cli containerd.io docker-compose-plugin

    # Add user to docker group
    limactl shell "$CLAUDE_VM_TEMPLATE" bash -c 'sudo usermod -aG docker $(whoami)'

    # Install Node.js 22 (needed for MCP servers)
    echo "Installing Node.js 22..."
    limactl shell "$CLAUDE_VM_TEMPLATE" bash -c "curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -"
    limactl shell "$CLAUDE_VM_TEMPLATE" sudo DEBIAN_FRONTEND=noninteractive apt-get install -y nodejs

    # Install Chromium and dependencies for headless browsing
    echo "Installing Chromium..."
    limactl shell "$CLAUDE_VM_TEMPLATE" sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
      chromium \
      fonts-liberation \
      xvfb
    # Symlink so tools looking for google-chrome find Chromium
    limactl shell "$CLAUDE_VM_TEMPLATE" sudo ln -sf /usr/bin/chromium /usr/bin/google-chrome
    limactl shell "$CLAUDE_VM_TEMPLATE" sudo ln -sf /usr/bin/chromium /usr/bin/google-chrome-stable
    limactl shell "$CLAUDE_VM_TEMPLATE" bash -c 'sudo mkdir -p /opt/google/chrome && sudo ln -sf /usr/bin/chromium /opt/google/chrome/chrome'

    # Install LSP servers for code intelligence
    echo "Installing LSP servers..."
    # C/C++: clangd
    limactl shell "$CLAUDE_VM_TEMPLATE" sudo DEBIAN_FRONTEND=noninteractive apt-get install -y clangd
    # Go: install Go toolchain and gopls
    limactl shell "$CLAUDE_VM_TEMPLATE" sudo DEBIAN_FRONTEND=noninteractive apt-get install -y golang-go
    limactl shell "$CLAUDE_VM_TEMPLATE" bash -c 'GOBIN=$HOME/.local/bin go install golang.org/x/tools/gopls@latest'
    # TypeScript/JavaScript: typescript-language-server
    limactl shell "$CLAUDE_VM_TEMPLATE" bash -c 'sudo npm install -g typescript-language-server typescript'
    # Python: pyright
    limactl shell "$CLAUDE_VM_TEMPLATE" bash -c 'sudo npm install -g pyright'
  fi

  # Install non-cloud kernel (cloud kernel lacks USB, serial, and other hardware drivers)
  echo "Installing non-cloud kernel..."
  limactl shell "$CLAUDE_VM_TEMPLATE" sudo DEBIAN_FRONTEND=noninteractive apt-get install -y linux-image-amd64
  limactl shell "$CLAUDE_VM_TEMPLATE" sudo bash -c '
    GRUB_CFG="/boot/grub/grub.cfg"
    SUBMENU_ID=$(grep -o "gnulinux-advanced-[a-f0-9-]*" "$GRUB_CFG" | head -1)
    ENTRY_ID=$(grep "menuentry " "$GRUB_CFG" | grep -v cloud | grep -o "gnulinux-[0-9][^ '\''\"]*" | head -1)
    if [ -n "$SUBMENU_ID" ] && [ -n "$ENTRY_ID" ]; then
      sed -i "s/^GRUB_DEFAULT=.*/GRUB_DEFAULT=\"${SUBMENU_ID}>${ENTRY_ID}\"/" /etc/default/grub
      update-grub
      echo "GRUB default set to: ${SUBMENU_ID}>${ENTRY_ID}"
    else
      echo "Warning: Could not determine non-cloud kernel GRUB entry" >&2
    fi
  '

  # Install mitmproxy (for transparent HTTPS interception)
  echo "Installing mitmproxy..."
  limactl shell "$CLAUDE_VM_TEMPLATE" bash -c 'command -v pip3 >/dev/null || sudo DEBIAN_FRONTEND=noninteractive apt-get install -y python3-pip'
  limactl shell "$CLAUDE_VM_TEMPLATE" sudo pip3 install --break-system-packages --ignore-installed bcrypt mitmproxy

  # Install Claude Code
  echo "Installing Claude Code..."
  limactl shell "$CLAUDE_VM_TEMPLATE" bash -c "curl -fsSL https://claude.ai/install.sh | bash"
  limactl shell "$CLAUDE_VM_TEMPLATE" bash -c 'echo "export PATH=\$HOME/.local/bin:\$HOME/.claude/local/bin:\$PATH" >> ~/.bashrc'

  # Skip first-run onboarding wizard (theme picker + login prompt)
  limactl shell "$CLAUDE_VM_TEMPLATE" bash -c '
    mkdir -p ~/.claude
    echo "{\"theme\":\"dark\",\"hasCompletedOnboarding\":true,\"skipDangerousModePermissionPrompt\":true,\"effortLevel\":\"high\"}" > ~/.claude/settings.json
  '

  # Pre-install official plugins marketplace and enable LSP plugins
  echo "Installing Claude Code LSP plugins..."
  limactl shell "$CLAUDE_VM_TEMPLATE" bash -c '
    MARKETPLACE_DIR="$HOME/.claude/plugins/marketplaces/claude-plugins-official"
    mkdir -p "$HOME/.claude/plugins/marketplaces"
    git clone --depth 1 https://github.com/anthropics/claude-plugins-official.git "$MARKETPLACE_DIR"
    cat > "$HOME/.claude/plugins/known_marketplaces.json" << MKTJSON
{"claude-plugins-official":{"source":{"source":"github","repo":"anthropics/claude-plugins-official"},"installLocation":"$MARKETPLACE_DIR","lastUpdated":"2026-01-01T00:00:00.000Z"}}
MKTJSON
    export PATH=$HOME/.local/bin:$HOME/.claude/local/bin:$PATH
    claude plugin install clangd-lsp@claude-plugins-official --scope user
    claude plugin install pyright-lsp@claude-plugins-official --scope user
    claude plugin install typescript-lsp@claude-plugins-official --scope user
    claude plugin install gopls-lsp@claude-plugins-official --scope user
  '

  # Install OpenCode
  echo "Installing OpenCode..."
  limactl shell "$CLAUDE_VM_TEMPLATE" bash -c "curl -fsSL https://opencode.ai/install | bash"
  # Symlink opencode into ~/.local/bin so it's on the default PATH
  # (the install script puts it in ~/.opencode/bin which isn't in PATH for non-interactive shells)
  limactl shell "$CLAUDE_VM_TEMPLATE" bash -c 'mkdir -p ~/.local/bin && ln -sf ~/.opencode/bin/opencode ~/.local/bin/opencode'

  if ! $minimal; then
    # Configure Chrome DevTools MCP server for Claude
    echo "Configuring Chrome MCP server..."
    limactl shell "$CLAUDE_VM_TEMPLATE" bash << 'VMEOF'
CONFIG="$HOME/.claude.json"
if [ -f "$CONFIG" ]; then
  jq '.mcpServers["chrome-devtools"] = {
    "command": "npx",
    "args": ["-y", "chrome-devtools-mcp@latest", "--headless=true", "--isolated=true"]
  }' "$CONFIG" > "$CONFIG.tmp" && mv "$CONFIG.tmp" "$CONFIG"
else
  cat > "$CONFIG" << 'JSON'
{
  "mcpServers": {
    "chrome-devtools": {
      "command": "npx",
      "args": ["-y", "chrome-devtools-mcp@latest", "--headless=true", "--isolated=true"]
    }
  }
}
JSON
fi
VMEOF
  fi

  # Pre-configure fuse for reverse-sshfs mounts (avoids setup delay on clone boot)
  limactl shell "$CLAUDE_VM_TEMPLATE" sudo bash -c '
    for conf in /etc/fuse.conf /etc/fuse3.conf; do
      if [ -e "$conf" ]; then
        grep -q "^user_allow_other" "$conf" || echo "user_allow_other" >> "$conf"
      fi
    done
  '

  # Disable SSH host key regeneration on clones (reuse template keys, saves ~1s per boot)
  limactl shell "$CLAUDE_VM_TEMPLATE" sudo bash -c '
    cat > /etc/cloud/cloud.cfg.d/99-lima-fast.cfg << CIEOF
ssh_deletekeys: false
ssh_genkeytypes: []
CIEOF
  '

  # Run user's custom setup script if it exists
  local user_setup="$HOME/.claude-vm.setup.sh"
  if [ -f "$user_setup" ]; then
    echo "Running custom setup from $user_setup..."
    limactl shell "$CLAUDE_VM_TEMPLATE" bash < "$user_setup"
  fi

  limactl stop "$CLAUDE_VM_TEMPLATE"

  echo "Template ready. Run 'agent-vm claude' or 'agent-vm opencode' in any project directory."
}

_claude_vm_build_credential_rules() {
  # Build CREDENTIAL_PROXY_RULES JSON from tokens.
  # Args: anthropic_token github_repos_json
  # github_repos_json: '{"owner/repo": "token", ...}' — per-repo tokens
  local anthropic_token="$1"
  local github_repos_json="$2"

  python3 -c "
import base64, json, sys
rules = []
anthropic_token = sys.argv[1]
repos = json.loads(sys.argv[2]) if sys.argv[2] else {}
# Always intercept api.anthropic.com (for upstream proxy routing via use_proxy).
# Inject Authorization only if a token is provided.
anthropic_headers = {}
if anthropic_token:
    anthropic_headers['Authorization'] = f'Bearer {anthropic_token}'
rules.append({
    'domain': 'api.anthropic.com',
    'headers': anthropic_headers,
    'use_proxy': True,
})
for slug, token in repos.items():
    owner, repo = slug.split('/', 1)
    basic = base64.b64encode(f'x-access-token:{token}'.encode()).decode()
    # git clone/fetch: github.com/owner/repo.git/...
    rules.append({
        'domain': 'github.com',
        'path_prefix': f'/{owner}/{repo}',
        'headers': {'Authorization': f'Basic {basic}'}
    })
    # gh CLI / API: api.github.com/repos/owner/repo/...
    rules.append({
        'domain': 'api.github.com',
        'path_prefix': f'/repos/{owner}/{repo}',
        'headers': {'Authorization': f'token {token}'}
    })
# Domain-level fallback using first token (for /user, /search, etc.)
if repos:
    first_token = next(iter(repos.values()))
    basic = base64.b64encode(f'x-access-token:{first_token}'.encode()).decode()
    rules.append({
        'domain': 'github.com',
        'headers': {'Authorization': f'Basic {basic}'}
    })
    rules.append({
        'domain': 'api.github.com',
        'headers': {'Authorization': f'token {first_token}'}
    })
print(json.dumps(rules))
" "$anthropic_token" "$github_repos_json"
}

_claude_vm_start_credential_proxy() {
  local rules_json="$1"
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

  # Generate per-instance secret for cross-VM isolation
  _credential_proxy_secret=$(python3 -c "import secrets; print(secrets.token_hex(32))")

  echo "Starting credential proxy..."
  exec 3< <(CREDENTIAL_PROXY_RULES="$rules_json" \
    CREDENTIAL_PROXY_SECRET="$_credential_proxy_secret" \
    CREDENTIAL_PROXY_DEBUG="${CREDENTIAL_PROXY_DEBUG:-0}" \
    CREDENTIAL_PROXY_LOG_DIR="${CREDENTIAL_PROXY_LOG_DIR:-.}" \
    python3 "$script_dir/credential-proxy.py")
  _credential_proxy_pid=$!
  if ! read -r -t 5 _credential_proxy_port <&3; then
    echo "Error: Credential proxy failed to start." >&2
    kill "$_credential_proxy_pid" 2>/dev/null
    exec 3<&-
    return 1
  fi
  exec 3<&-
  echo "Credential proxy listening on port $_credential_proxy_port"
}

_claude_vm_setup_mitmproxy() {
  local vm_name="$1"

  # Generate mitmproxy CA cert and install in system trust store
  limactl shell "$vm_name" bash -c '
    # Generate CA if not already present
    if [ ! -f ~/.mitmproxy/mitmproxy-ca-cert.pem ]; then
      # Run mitmdump briefly to generate CA certs
      timeout 2 mitmdump --listen-port 0 2>/dev/null || true
    fi

    # Install CA in system trust store
    sudo cp ~/.mitmproxy/mitmproxy-ca-cert.pem /usr/local/share/ca-certificates/mitmproxy-ca.crt
    sudo update-ca-certificates

    # Configure git to use system CA bundle (includes mitmproxy CA)
    git config --global http.sslCAInfo /etc/ssl/certs/ca-certificates.crt
  '

  # Set HTTPS_PROXY env var for all processes via profile.d
  # (not .bashrc — avoids polluting the shell after mitmproxy stops)
  limactl shell "$vm_name" bash -c '
    sudo tee /etc/profile.d/credential-proxy.sh > /dev/null <<EOF
export HTTPS_PROXY=http://127.0.0.1:8080
export HTTP_PROXY=http://127.0.0.1:8080
export https_proxy=http://127.0.0.1:8080
export http_proxy=http://127.0.0.1:8080
export NODE_EXTRA_CA_CERTS=/etc/ssl/certs/ca-certificates.crt
EOF
    sudo chmod 644 /etc/profile.d/credential-proxy.sh
  '
}

_claude_vm_start_mitmproxy() {
  local vm_name="$1"
  local credential_proxy_port="$2"
  local intercepted_domains="$3"  # comma-separated: "api.anthropic.com,github.com,api.github.com"
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

  # Copy addon script to VM
  limactl shell "$vm_name" bash -c 'mkdir -p ~/.mitmproxy'
  cat "$script_dir/mitmproxy-addon.py" | limactl shell "$vm_name" bash -c 'cat > ~/.mitmproxy/addon.py'

  # Write a launcher script into the VM, then start it via systemd-run.
  # Direct "nohup ... &" inside "limactl shell" doesn't survive the SSH session exit.
  limactl shell "$vm_name" bash -c "
    cat > /tmp/start-mitmproxy.sh << 'LAUNCHER'
#!/bin/bash
exec mitmdump \
  --listen-port 8080 \
  --set connection_strategy=lazy \
  -s ~/.mitmproxy/addon.py
LAUNCHER
    chmod +x /tmp/start-mitmproxy.sh
  "

  # Start as a systemd user service so it survives the limactl shell session
  limactl shell "$vm_name" bash -c "
    systemd-run --user --unit=mitmproxy \
      --setenv=CREDENTIAL_PROXY_HOST=host.lima.internal \
      --setenv=CREDENTIAL_PROXY_PORT=$credential_proxy_port \
      --setenv=CREDENTIAL_PROXY_SECRET='$_credential_proxy_secret' \
      --setenv=CREDENTIAL_PROXY_DOMAINS='$intercepted_domains' \
      --setenv=BLOCKED_DOMAINS='datadoghq.com' \
      /tmp/start-mitmproxy.sh
  "

  # Wait for mitmproxy to be ready (single session: launch + wait)
  limactl shell "$vm_name" bash -c '
    for i in $(seq 1 30); do
      if bash -c "echo >/dev/tcp/127.0.0.1/8080" 2>/dev/null; then
        echo "mitmproxy ready"
        exit 0
      fi
      sleep 0.2
    done
    echo "ERROR: mitmproxy failed to start. Logs:" >&2
    journalctl --user -u mitmproxy --no-pager -n 20 >&2
    exit 1
  '
}

_claude_vm_inject_git_credentials() {
  local vm_name="$1"

  # Get git user identity from host
  local git_name git_email
  git_name=$(git config user.name 2>/dev/null || echo "")
  git_email=$(git config user.email 2>/dev/null || echo "")

  # Set up git credential helper with a placeholder token.
  # The real token is injected by the host credential proxy via mitmproxy.
  limactl shell "$vm_name" bash -c "
    git config --global credential.helper store
    # Store a placeholder credential for github.com
    # (git will send this, mitmproxy intercepts, host proxy overwrites with real token)
    mkdir -p \$HOME
    echo 'https://x-access-token:placeholder@github.com' > \$HOME/.git-credentials
    chmod 600 \$HOME/.git-credentials
    # Rewrite SSH URLs to HTTPS so all git traffic goes through mitmproxy
    git config --global url.\"https://github.com/\".insteadOf \"git@github.com:\"
    ${git_name:+git config --global user.name \"$git_name\"}
    ${git_email:+git config --global user.email \"$git_email\"}
  "
}

_claude_vm_inject_gh_credentials() {
  local vm_name="$1"

  # Write gh config with placeholder token.
  # Real auth is injected by the host credential proxy via mitmproxy.
  # config.yml must have version: "1" to prevent gh >=2.40 from attempting
  # a multi-account migration that calls the GitHub API (which fails without proxy).
  limactl shell "$vm_name" bash -c '
    mkdir -p "$HOME/.config/gh"
    cat > "$HOME/.config/gh/config.yml" << '\''CONFIG'\''
version: "1"
git_protocol: https
CONFIG
    cat > "$HOME/.config/gh/hosts.yml" << '\''HOSTS'\''
github.com:
    oauth_token: placeholder-token-injected-by-proxy
    user: x-access-token
    git_protocol: https
HOSTS
  '
}

_claude_vm_write_instructions() {
  local vm_name="$1"
  local repos_json="$2"  # JSON dict: {"owner/repo": "token", ...} or "{}"

  local repo_list
  repo_list=$(python3 -c "
import json, sys
repos = json.loads(sys.argv[1])
slugs = list(repos.keys())
if not slugs:
    print('No GitHub repos have credentials configured.')
elif len(slugs) == 1:
    print(f'Only {slugs[0]} has credentials configured. Other repos will require')
    print('their own authentication.')
else:
    print('The following repositories have credentials configured:')
    for s in slugs:
        print(f'  - {s}')
    print('Other repos will require their own authentication.')
" "$repos_json")

  limactl shell "$vm_name" bash -c "
    mkdir -p \$HOME/.claude
    cat >> \$HOME/.claude/CLAUDE.md << INSTRUCTIONS

# Git access

Git push and pull to GitHub work out of the box for this repository.
You can use standard git commands:

    git push origin main
    git push origin HEAD:my-branch
    git pull origin main

The \\\`gh\\\` CLI also works and is pre-authenticated. You can use it for
pull requests, issues, and other GitHub operations:

    gh pr list
    gh pr create --title \"...\" --body \"...\"
    gh issue list

${repo_list}
INSTRUCTIONS
  "
}

_claude_vm_inject_clipboard_shim() {
  local vm_name="$1"
  local state_dir="$2"
  limactl shell "$vm_name" bash -c '
    mkdir -p ~/.local/bin
    cat > ~/.local/bin/wl-paste << '\''SHIM'\''
#!/bin/sh
# Shim for clipboard image paste from host via shared mount.
# Claude Code calls: wl-paste -l (list types), wl-paste --type image/png (read)
CLIPBOARD_FILE="'"$state_dir"'/clipboard.png"
for arg in "$@"; do
  case "$arg" in
    -l|--list-types)
      if [ -f "$CLIPBOARD_FILE" ]; then
        echo "image/png"
        exit 0
      else
        exit 1
      fi
      ;;
  esac
done
# Default: output the image data
if [ -f "$CLIPBOARD_FILE" ]; then
  exec cat "$CLIPBOARD_FILE"
else
  exit 1
fi
SHIM
    chmod +x ~/.local/bin/wl-paste
    cat > ~/.local/bin/xclip << '\''XSHIM'\''
#!/bin/sh
# Shim for clipboard image paste from host via shared mount.
# Claude Code calls: xclip -selection clipboard -t TARGETS -o (list types)
#                    xclip -selection clipboard -t image/png -o (read)
CLIPBOARD_FILE="'"$state_dir"'/clipboard.png"
has_targets=false
has_output=false
mime_type=""
for arg in "$@"; do
  case "$arg" in
    TARGETS) has_targets=true ;;
    -o) has_output=true ;;
    image/*) mime_type="$arg" ;;
  esac
done
if $has_targets && $has_output; then
  if [ -f "$CLIPBOARD_FILE" ]; then
    echo "image/png"
    exit 0
  else
    exit 1
  fi
fi
if $has_output; then
  if [ -f "$CLIPBOARD_FILE" ]; then
    exec cat "$CLIPBOARD_FILE"
  else
    exit 1
  fi
fi
exit 1
XSHIM
    chmod +x ~/.local/bin/xclip
  '
}

_claude_vm_write_oauth_token() {
  local vm_name="$1"
  local token="${2:-placeholder}"
  # Set CLAUDE_CODE_OAUTH_TOKEN so Claude Code authenticates via the proxy.
  # Real auth header is injected by the host credential proxy.
  limactl shell "$vm_name" bash -c "
    sudo tee /etc/profile.d/claude-oauth.sh > /dev/null <<EOF
export CLAUDE_CODE_OAUTH_TOKEN=$token
EOF
    sudo chmod 644 /etc/profile.d/claude-oauth.sh
  "
}

_claude_vm_setup_session_persistence() {
  local vm_name="$1"
  local state_dir="$2"
  limactl shell "$vm_name" bash -c '
    SESSION_DIR="'"$state_dir"'/claude-sessions"
    mkdir -p ~/.claude \
      "$SESSION_DIR/projects" \
      "$SESSION_DIR/file-history" \
      "$SESSION_DIR/todos" \
      "$SESSION_DIR/plans"
    ln -sfn "$SESSION_DIR/projects" ~/.claude/projects
    ln -sfn "$SESSION_DIR/file-history" ~/.claude/file-history
    ln -sfn "$SESSION_DIR/todos" ~/.claude/todos
    ln -sfn "$SESSION_DIR/plans" ~/.claude/plans
    touch "$SESSION_DIR/history.jsonl"
    ln -sfn "$SESSION_DIR/history.jsonl" ~/.claude/history.jsonl

    if [ ! -f "$SESSION_DIR/claude.json" ]; then
      if [ -f ~/.claude.json ] && [ ! -L ~/.claude.json ]; then
        cp ~/.claude.json "$SESSION_DIR/claude.json"
      else
        echo '{}' > "$SESSION_DIR/claude.json"
      fi
    fi
    ln -sfn "$SESSION_DIR/claude.json" ~/.claude.json
  '
}

_claude_vm_ensure_onboarding_config() {
  local vm_name="$1"
  local host_dir="$2"
  limactl shell "$vm_name" bash -c '
    CONFIG="$HOME/.claude.json"
    SETTINGS="$HOME/.claude/settings.json"
    PROJECT_PATH="'"$host_dir"'"

    mkdir -p "$HOME/.claude"

    if [ ! -f "$SETTINGS" ]; then
      echo "{}" > "$SETTINGS"
    fi

    jq ".hasCompletedOnboarding = true | .skipDangerousModePermissionPrompt = true" \
      "$SETTINGS" > "$SETTINGS.tmp" && mv "$SETTINGS.tmp" "$SETTINGS"

    if [ ! -f "$CONFIG" ]; then
      echo "{}" > "$CONFIG"
    fi

    jq --arg project_path "$PROJECT_PATH" \
      ".hasCompletedOnboarding = true
       | .lastOnboardingVersion = (.lastOnboardingVersion // \"vm\")
       | .effortCalloutDismissed = true
       | .projects = (.projects // {})
       | .projects[
           \$project_path
         ] = ((.projects[\$project_path] // {}) + {hasTrustDialogAccepted: true})" \
      "$CONFIG" > "$CONFIG.tmp" && mv "$CONFIG.tmp" "$CONFIG"
  '
}

_claude_vm_parse_github_remote() {
  # Parse owner/repo from a git remote URL. Prints "owner repo" or returns 1.
  python3 -c "
import re, sys
url = sys.argv[1]
for pat in [r'git@github\.com:([^/]+)/([^/]+?)(?:\.git)?$',
            r'https?://github\.com/([^/]+)/([^/]+?)(?:\.git)?(?:/.*)?$']:
    m = re.match(pat, url)
    if m:
        print(m.group(1), m.group(2))
        sys.exit(0)
sys.exit(1)
" "$1"
}

_claude_vm_check_push_access() {
  # Check if the user has push access to a GitHub repo via git push --dry-run.
  # Works with both SSH and HTTPS host credentials. No API token needed.
  # Returns 0 (true) if push access, 1 (false) if no push access or no local clone.
  local host_dir="$1"
  [ -z "$host_dir" ] && return 1

  local push_output
  push_output=$(git -C "$host_dir" push --dry-run --no-verify origin HEAD 2>&1) && return 0
  # "non-fast-forward" or "up to date" = have push access (server rejected the ref, not the auth)
  echo "$push_output" | grep -qiE 'non-fast-forward|up to date|Everything up-to-date' && return 0
  return 1
}

_claude_vm_get_github_token() {
  # Acquire a GitHub token for the project and its submodules.
  # Sets _claude_vm_github_token (first repo's token) and _claude_vm_github_repos_json.
  local host_dir="$1"
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

  local repos_json='{}'
  local owner="" repo=""
  _claude_vm_github_token=""

  # Detect main repo from git remote
  local repo_url
  repo_url=$(git -C "$host_dir" remote get-url origin 2>/dev/null)
  if [ -n "$repo_url" ]; then
    read -r owner repo < <(_claude_vm_parse_github_remote "$repo_url") || true
    if [ -n "$owner" ] && [ -n "$repo" ]; then
      # Check push access before starting device auth flow
      if ! _claude_vm_check_push_access "$host_dir"; then
        echo "  $owner/$repo: no write access, skipping token request"
      else
        echo "Requesting GitHub token for $owner/$repo..."
        local token
        token=$(python3 "$script_dir/github_app_token_demo.py" \
          user-token --client-id Iv23liisR1WdpJmDUPLT \
          --repo "$repo_url" --token-only \
          --cache-dir "$HOME/.cache/claude-vm") || true
        if [ -n "$token" ]; then
          _claude_vm_github_token="$token"
          repos_json=$(python3 -c "import json; print(json.dumps({__import__('sys').argv[1]: __import__('sys').argv[2]}))" \
            "$owner/$repo" "$token")
        else
          echo "  Warning: no token for $owner/$repo"
        fi
      fi
    fi
  fi

  # Detect GitHub submodules from .gitmodules
  if [ -f "$host_dir/.gitmodules" ]; then
    local sub_entries
    # Print "url<TAB>path" for each GitHub submodule
    sub_entries=$(python3 -c "
import configparser, sys
p = configparser.ConfigParser()
p.read(sys.argv[1])
for s in p.sections():
    url = p.get(s, 'url', fallback='')
    path = p.get(s, 'path', fallback='')
    if 'github.com' in url:
        print(f'{url}\t{path}')
" "$host_dir/.gitmodules")
    local sub_url sub_path sub_owner sub_repo sub_token sub_dir
    while IFS=$'\t' read -r sub_url sub_path; do
      [ -z "$sub_url" ] && continue
      read -r sub_owner sub_repo < <(_claude_vm_parse_github_remote "$sub_url") || continue
      [ -z "$sub_owner" ] || [ -z "$sub_repo" ] && continue
      # Skip if same as main repo
      [ -n "$owner" ] && [ "$sub_owner/$sub_repo" = "$owner/$repo" ] && continue

      # Check push access before requesting a scoped token
      sub_dir=""
      [ -n "$sub_path" ] && [ -d "$host_dir/$sub_path/.git" ] && sub_dir="$host_dir/$sub_path"
      if ! _claude_vm_check_push_access "$sub_dir"; then
        echo "  Submodule $sub_owner/$sub_repo: no write access, skipping token request"
        continue
      fi

      echo "Requesting GitHub token for submodule $sub_owner/$sub_repo..."
      sub_token=$(python3 "$script_dir/github_app_token_demo.py" \
        user-token --client-id Iv23liisR1WdpJmDUPLT \
        --repo "$sub_url" --token-only \
        --cache-dir "$HOME/.cache/claude-vm") || true
      if [ -n "$sub_token" ]; then
        repos_json=$(python3 -c "
import json, sys
d = json.loads(sys.argv[1])
d[sys.argv[2]] = sys.argv[3]
print(json.dumps(d))
" "$repos_json" "$sub_owner/$sub_repo" "$sub_token")
        echo "  Got token for $sub_owner/$sub_repo"
        # Use first available token if main repo didn't have one
        [ -z "$_claude_vm_github_token" ] && _claude_vm_github_token="$sub_token"
      else
        echo "  Warning: no token for $sub_owner/$sub_repo (skipping credential injection)"
      fi
    done <<< "$sub_entries"
  fi

  # If no repos were configured, skip GitHub integration
  if [ "$repos_json" = '{}' ]; then
    echo "Warning: No GitHub repos found (no remote, no submodules), skipping GitHub" >&2
    _claude_vm_github_repos_json='{}'
    return 1
  fi

  local _scope_list
  _scope_list=$(python3 -c "import json,sys; print(', '.join(json.loads(sys.argv[1]).keys()))" "$repos_json")
  echo "GitHub token acquired (scope: $_scope_list)"

  _claude_vm_github_repos_json="$repos_json"
}


_claude_vm_security_snapshot() {
  local host_dir="$1"
  local snapshot_file="$2"
  # Backup .git/config for potential restoration
  if [ -f "${host_dir}/.git/config" ]; then
    cp "${host_dir}/.git/config" "${snapshot_file}.git-config"
  fi
  {
    find "${host_dir}/.git/hooks" -type f -exec shasum {} \; 2>/dev/null
    shasum "${host_dir}/.git/config" 2>/dev/null
    for f in .claude-vm.runtime.sh CLAUDE.md Makefile; do
      if [ -f "${host_dir}/${f}" ]; then
        shasum "${host_dir}/${f}"
      else
        echo "ABSENT  ${host_dir}/${f}"
      fi
    done
  } | sort > "$snapshot_file"
}

_claude_vm_security_check() {
  local host_dir="$1"
  local snapshot_file="$2"
  local after_file
  after_file="$(mktemp)"
  _claude_vm_security_snapshot "$host_dir" "$after_file"

  if ! diff -q "$snapshot_file" "$after_file" >/dev/null 2>&1; then
    echo ""
    echo "⚠️  Security: files modified by the VM session:"
    diff "$snapshot_file" "$after_file" | grep '^[<>]' | sed 's/^</  Removed:/;s/^>/  Added\/Changed:/'
    echo ""

    # Check specifically for new/modified hooks
    local new_hooks
    new_hooks=$(diff "$snapshot_file" "$after_file" | grep '^>' | grep '\.git/hooks' | awk '{print $NF}')
    if [ -n "$new_hooks" ]; then
      echo "⚠️  WARNING: New/modified git hooks detected (these run on YOUR machine):"
      while read -r hook; do
        echo "    $hook"
      done <<< "$new_hooks"
      echo ""
      read -r -p "Remove these hooks? [Y/n] " answer
      if [[ -z "$answer" || "$answer" =~ ^[Yy] ]]; then
        while read -r hook; do
          rm -f "$hook"
          echo "  Removed: $hook"
        done <<< "$new_hooks"
      fi
    fi

    # Check for .git/config changes
    if diff "$snapshot_file" "$after_file" | grep -q '\.git/config'; then
      echo "⚠️  WARNING: .git/config was modified during the VM session."
      echo "    ${host_dir}/.git/config"
      local config_backup="${snapshot_file}.git-config"
      # Restore .git/config from the pre-session backup if the user agrees
      if [ -f "$config_backup" ]; then
        read -r -p "Restore .git/config from pre-session backup? [Y/n] " answer
        if [[ -z "$answer" || "$answer" =~ ^[Yy] ]]; then
          cp "$config_backup" "${host_dir}/.git/config"
          echo "  Restored: ${host_dir}/.git/config"
        fi
      fi
    fi
  fi
  rm -f "$after_file" "${after_file}.git-config"
}

# ── OpenCode helpers ──────────────────────────────────────────────────────────

_opencode_vm_setup_config() {
  local vm_name="$1"  # unused but kept for consistent API
  local state_dir="$2"

  local config_dir="${state_dir}/opencode-config"
  mkdir -p "$config_dir"

  # Use real upstream URL — mitmproxy intercepts HTTPS and the host credential
  # proxy injects the real API key. The dummy apiKey satisfies OpenCode's config
  # validation but is overwritten by the proxy.
  cat > "${config_dir}/opencode.json" << 'OCJSON'
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "anthropic": {
      "options": {
        "baseURL": "https://api.anthropic.com/v1",
        "apiKey": "dummy-key-auth-handled-by-proxy"
      }
    }
  },
  "permission": "allow",
  "autoupdate": false,
  "watcher": {
    "ignore": ["node_modules/**", "dist/**", ".git/**", ".agent-vm/**"]
  },
  "mcp": {
    "chrome-devtools": {
      "type": "local",
      "command": ["npx", "-y", "chrome-devtools-mcp@latest", "--headless=true", "--isolated=true"],
      "enabled": true
    }
  }
}
OCJSON
}

_opencode_vm_setup_auth() {
  local vm_name="$1"
  # Write a dummy auth.json so OpenCode recognizes the Anthropic provider
  # as configured.  The actual API key is set via provider.anthropic.options.apiKey
  # in opencode.json, and real auth is handled by the host proxy.
  limactl shell "$vm_name" bash -c '
    mkdir -p ~/.local/share/opencode
    cat > ~/.local/share/opencode/auth.json << '\''AUTH'\''
{
  "anthropic": {
    "apiKey": "dummy-key-auth-handled-by-proxy"
  }
}
AUTH
  '
}

_opencode_vm_setup_session_persistence() {
  local vm_name="$1"
  local state_dir="$2"
  limactl shell "$vm_name" bash -c '
    SESSION_DIR="'"$state_dir"'/opencode-sessions"
    mkdir -p "$SESSION_DIR" \
      ~/.local/share/opencode
    if [ -d ~/.local/share/opencode ] && [ ! -L ~/.local/share/opencode ]; then
      cp -a ~/.local/share/opencode/. "$SESSION_DIR/" 2>/dev/null || true
      rm -rf ~/.local/share/opencode
    fi
    ln -sfn "$SESSION_DIR" ~/.local/share/opencode
  '
}


_agent_vm_run() {
  local agent="$1"
  shift
  local use_github=true
  local usb_devices=()
  local memory=""
  local max_memory=""
  local args=()
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --no-git) use_github=false; shift ;;
      --usb) usb_devices+=("$2"); shift 2 ;;
      --usb=*) usb_devices+=("${1#*=}"); shift ;;
      --memory) memory="$2"; shift 2 ;;
      --memory=*) memory="${1#*=}"; shift ;;
      --max-memory) max_memory="$2"; shift 2 ;;
      --max-memory=*) max_memory="${1#*=}"; shift ;;
      *) args+=("$1"); shift ;;
    esac
  done
  local vm_name="${agent}-$(basename "$(pwd)" | tr -cs 'a-zA-Z0-9' '-' | sed 's/^-//;s/-$//')-$$"
  local host_dir="$(pwd)"
  local state_dir
  state_dir="$(_agent_vm_project_state_dir "$host_dir")"
  local _usb_sysfs_ports=()
  local _usb_qemu_wrapper=""
  local _balloon_daemon_pid=""

  if ! limactl list -q 2>/dev/null | grep -q "^${CLAUDE_VM_TEMPLATE}$"; then
    echo "Error: Template VM not found. Run 'agent-vm setup' first." >&2
    return 1
  fi

  # Set up cleanup trap before starting any background processes so Ctrl+C
  # at any point (proxy start, clone, boot, etc.) cleans up everything.
  local security_snapshot=""
  _claude_vm_cleanup() {
    echo "Cleaning up VM..."
    [ -n "$_balloon_daemon_pid" ] && kill "$_balloon_daemon_pid" 2>/dev/null
    [ -n "$_credential_proxy_pid" ] && kill "$_credential_proxy_pid" 2>/dev/null
    limactl stop "$vm_name" &>/dev/null
    limactl delete "$vm_name" --force &>/dev/null
    # Fall back to rm if limactl delete left a broken directory (missing lima.yaml
    # breaks all future limactl commands)
    [ -d "$HOME/.lima/$vm_name" ] && rm -rf "$HOME/.lima/$vm_name"
    [ -n "$security_snapshot" ] && rm -f "$security_snapshot" "${security_snapshot}.git-config"
    local port
    for port in "${_usb_sysfs_ports[@]}"; do
      _agent_vm_usb_rebind "$port"
    done
    [ -n "$_usb_qemu_wrapper" ] && rm -f "$_usb_qemu_wrapper"
  }
  trap _claude_vm_cleanup EXIT INT TERM

  # Get Anthropic token
  local _anthropic_token="${CLAUDE_VM_PROXY_ACCESS_TOKEN:-}"

  # Get GitHub token
  _claude_vm_github_repos_json='{}'
  _claude_vm_github_token=""
  if $use_github; then
    _claude_vm_get_github_token "$host_dir" || echo "Continuing without GitHub..."
  fi

  # Build credential rules and start unified proxy (if any credentials configured)
  local _credential_rules
  _credential_rules=$(_claude_vm_build_credential_rules "$_anthropic_token" "$_claude_vm_github_repos_json")
  local _credential_proxy_port=""
  if [ "$_credential_rules" != "[]" ]; then
    _claude_vm_start_credential_proxy "$_credential_rules" || { _claude_vm_cleanup; trap - EXIT INT TERM; return 1; }
  fi

  # Security: snapshot before session
  security_snapshot="$(mktemp)"
  _claude_vm_security_snapshot "$host_dir" "$security_snapshot"

  # Ensure .git/hooks exists for read-only mount
  mkdir -p "${host_dir}/.git/hooks"

  echo "Starting VM '$vm_name'..."
  # Mount project dir writable, but .git/hooks read-only at the Lima level (VM root cannot override)
  # Note: .git/config is a file (not a directory) so it cannot be a Lima mount;
  # it is protected via the pre/post-session security check instead.
  limactl clone "$CLAUDE_VM_TEMPLATE" "$vm_name" \
    --set ".mounts=[{\"location\":\"${host_dir}\",\"writable\":true},{\"location\":\"${host_dir}/.git/hooks\",\"writable\":false},{\"location\":\"${state_dir}\",\"writable\":true}]" \
    --set '.containerd.system=false' \
    --set '.containerd.user=false' \
    --tty=false &>/dev/null

  # USB passthrough: resolve devices, unbind host drivers
  local _usb_vidpids=()
  if [ ${#usb_devices[@]} -gt 0 ]; then
    local dev vidpid port
    for dev in "${usb_devices[@]}"; do
      vidpid="$(_agent_vm_usb_resolve "$dev")" || { _claude_vm_cleanup; trap - EXIT INT TERM; return 1; }
      port="$(_agent_vm_usb_find_sysfs "$vidpid")" || { _claude_vm_cleanup; trap - EXIT INT TERM; return 1; }
      echo "USB: $dev → $vidpid (port $port)"
      _agent_vm_usb_unbind "$port"
      _usb_sysfs_ports+=("$port")
      _usb_vidpids+=("$vidpid")
    done
  fi

  # Start VM — with virtio-balloon if QEMU is available (Linux), plain otherwise
  local _has_balloon=false
  if command -v qemu-system-x86_64 &>/dev/null; then
    # Override memory ceiling if --max-memory was specified
    if [ -n "$max_memory" ]; then
      limactl edit "$vm_name" --memory="$max_memory" --tty=false &>/dev/null
    fi
    if [ ${#_usb_vidpids[@]} -gt 0 ]; then
      _usb_qemu_wrapper="$(_agent_vm_qemu_wrapper "${_usb_vidpids[@]}")"
    else
      _usb_qemu_wrapper="$(_agent_vm_qemu_wrapper)"
    fi
    if QEMU_SYSTEM_X86_64="$_usb_qemu_wrapper" limactl start "$vm_name" &>/dev/null; then
      _has_balloon=true
    else
      # Balloon may have broken QEMU (e.g. PCI conflict); retry without it
      echo "Warning: VM failed to start with balloon, retrying without..." >&2
      limactl stop "$vm_name" &>/dev/null
      rm -f "$_usb_qemu_wrapper"
      if [ ${#_usb_vidpids[@]} -gt 0 ]; then
        _usb_qemu_wrapper="$(_agent_vm_qemu_wrapper --no-balloon "${_usb_vidpids[@]}")"
        QEMU_SYSTEM_X86_64="$_usb_qemu_wrapper" limactl start "$vm_name" &>/dev/null
      else
        _usb_qemu_wrapper=""
        limactl start "$vm_name" &>/dev/null
      fi
    fi

    # Start balloon daemon only if balloon device is present
    if $_has_balloon; then
      local _balloon_script
      _balloon_script="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/balloon-daemon.py"
      local _qmp_sock="$HOME/.lima/$vm_name/qmp.sock"
      local _balloon_daemon_args=()
      if [ -n "$max_memory" ]; then
        _balloon_daemon_args+=(--max-memory "${max_memory}G")
      fi
      python3 "$_balloon_script" "$_qmp_sock" daemon "${_balloon_daemon_args[@]}" &>/dev/null &
      _balloon_daemon_pid=$!
    fi
  else
    if [ -n "$max_memory" ]; then
      echo "Warning: --max-memory ignored without balloon (QEMU not available)" >&2
    fi
    if [ -n "$memory" ]; then
      limactl edit "$vm_name" --memory="$memory" --tty=false &>/dev/null
    fi
    limactl start "$vm_name" &>/dev/null
  fi

  _claude_vm_write_oauth_token "$vm_name" "$_anthropic_token"
  _claude_vm_inject_clipboard_shim "$vm_name" "$state_dir"
  _claude_vm_setup_session_persistence "$vm_name" "$state_dir"
  _claude_vm_ensure_onboarding_config "$vm_name" "$host_dir"

  # Start proxy chain only if there are credential rules
  if [ -n "$_credential_rules" ] && [ "$_credential_rules" != "[]" ]; then
    # Set up mitmproxy (CA cert, system trust, proxy env vars)
    echo "Setting up mitmproxy..."
    _claude_vm_setup_mitmproxy "$vm_name"

    # Build intercepted domains list from credential rules
    local _intercepted_domains
    _intercepted_domains=$(python3 -c "
import json, sys
rules = json.loads(sys.argv[1])
seen = dict.fromkeys(r['domain'] for r in rules)
print(','.join(seen))
" "$_credential_rules")

    _claude_vm_start_mitmproxy "$vm_name" "$_credential_proxy_port" "$_intercepted_domains"
  fi

  if $use_github && [ "$_claude_vm_github_repos_json" != '{}' ]; then
    _claude_vm_inject_git_credentials "$vm_name"
    _claude_vm_inject_gh_credentials "$vm_name"
    _claude_vm_write_instructions "$vm_name" "$_claude_vm_github_repos_json"
  fi

  if [ "$agent" = "opencode" ]; then
    _opencode_vm_setup_config "$vm_name" "$state_dir"
    _opencode_vm_setup_session_persistence "$vm_name" "$state_dir"
    _opencode_vm_setup_auth "$vm_name"
  fi

  # Run project-specific runtime script if it exists
  if [ -f "${host_dir}/.claude-vm.runtime.sh" ]; then
    echo "Running project runtime setup..."
    limactl shell --workdir "$host_dir" "$vm_name" bash -l < "${host_dir}/.claude-vm.runtime.sh"
  fi

  # Update the agent tool to latest version before launching
  if [ "$agent" = "opencode" ]; then
    echo "Updating OpenCode..."
    limactl shell "$vm_name" bash -lc 'opencode update 2>/dev/null || true'
  else
    echo "Updating Claude Code..."
    limactl shell "$vm_name" bash -lc 'claude update --yes 2>/dev/null || true'
  fi

  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  if [ "$agent" = "opencode" ]; then
    CLIPBOARD_DIR="$state_dir" python3 "$script_dir/clipboard-pty.py" \
      limactl shell --workdir "$host_dir" "$vm_name" \
      env OPENCODE_CONFIG="${state_dir}/opencode-config/opencode.json" \
      opencode "${args[@]}"
  else
    local claude_args=("${args[@]}")
    CLIPBOARD_DIR="$state_dir" python3 "$script_dir/clipboard-pty.py" \
      limactl shell --workdir "$host_dir" "$vm_name" \
      env IS_SANDBOX=1 \
      ENABLE_LSP_TOOL=1 \
      claude --dangerously-skip-permissions "${claude_args[@]}"
  fi
  _claude_vm_security_check "$host_dir" "$security_snapshot"

  _claude_vm_cleanup
  trap - EXIT INT TERM
}

_agent_vm_shell() {
  local agent=""
  local use_github=true

  if [[ $# -gt 0 && "$1" != --* ]]; then
    agent="$1"
    shift
  fi

  case "$agent" in
    ""|claude|opencode) ;;
    *)
      echo "Error: Unknown agent '$agent' for shell. Use: claude or opencode." >&2
      return 1
      ;;
  esac

  local usb_devices=()
  local memory=""
  local max_memory=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --no-git) use_github=false; shift ;;
      --usb) usb_devices+=("$2"); shift 2 ;;
      --usb=*) usb_devices+=("${1#*=}"); shift ;;
      --memory) memory="$2"; shift 2 ;;
      --memory=*) memory="${1#*=}"; shift ;;
      --max-memory) max_memory="$2"; shift 2 ;;
      --max-memory=*) max_memory="${1#*=}"; shift ;;
      *) shift ;;
    esac
  done
  local vm_name="${agent:+${agent}-}debug-$$"
  local host_dir="$(pwd)"
  local state_dir
  state_dir="$(_agent_vm_project_state_dir "$host_dir")"
  local _usb_sysfs_ports=()
  local _usb_qemu_wrapper=""
  local _balloon_daemon_pid=""

  if ! limactl list -q 2>/dev/null | grep -q "^${CLAUDE_VM_TEMPLATE}$"; then
    echo "Error: Template VM not found. Run 'agent-vm setup' first." >&2
    return 1
  fi

  # Set up cleanup trap before starting any background processes so Ctrl+C
  # at any point (proxy start, clone, boot, etc.) cleans up everything.
  local security_snapshot=""
  _claude_vm_shell_cleanup() {
    echo "Cleaning up VM..."
    [ -n "$_balloon_daemon_pid" ] && kill "$_balloon_daemon_pid" 2>/dev/null
    [ -n "$_credential_proxy_pid" ] && kill "$_credential_proxy_pid" 2>/dev/null
    limactl stop "$vm_name" &>/dev/null
    limactl delete "$vm_name" --force &>/dev/null
    # Fall back to rm if limactl delete left a broken directory (missing lima.yaml
    # breaks all future limactl commands)
    [ -d "$HOME/.lima/$vm_name" ] && rm -rf "$HOME/.lima/$vm_name"
    [ -n "$security_snapshot" ] && rm -f "$security_snapshot" "${security_snapshot}.git-config"
    local port
    for port in "${_usb_sysfs_ports[@]}"; do
      _agent_vm_usb_rebind "$port"
    done
    [ -n "$_usb_qemu_wrapper" ] && rm -f "$_usb_qemu_wrapper"
  }
  trap _claude_vm_shell_cleanup EXIT INT TERM

  # Get Anthropic token
  local _anthropic_token="${CLAUDE_VM_PROXY_ACCESS_TOKEN:-}"

  # Get GitHub token
  _claude_vm_github_repos_json='{}'
  _claude_vm_github_token=""
  if $use_github; then
    _claude_vm_get_github_token "$host_dir" || echo "Continuing without GitHub..."
  fi

  # Build credential rules and start unified proxy (if any credentials configured)
  local _credential_rules
  _credential_rules=$(_claude_vm_build_credential_rules "$_anthropic_token" "$_claude_vm_github_repos_json")
  local _credential_proxy_port=""
  if [ "$_credential_rules" != "[]" ]; then
    _claude_vm_start_credential_proxy "$_credential_rules" || { _claude_vm_shell_cleanup; trap - EXIT INT TERM; return 1; }
  fi

  # Security: snapshot before session
  security_snapshot="$(mktemp)"
  _claude_vm_security_snapshot "$host_dir" "$security_snapshot"

  # Ensure .git/hooks exists for read-only mount
  mkdir -p "${host_dir}/.git/hooks"

  # Mount project dir writable, but .git/hooks read-only at the Lima level (VM root cannot override)
  limactl clone "$CLAUDE_VM_TEMPLATE" "$vm_name" \
    --set ".mounts=[{\"location\":\"${host_dir}\",\"writable\":true},{\"location\":\"${host_dir}/.git/hooks\",\"writable\":false},{\"location\":\"${state_dir}\",\"writable\":true}]" \
    --set '.containerd.system=false' \
    --set '.containerd.user=false' \
    --tty=false &>/dev/null

  # USB passthrough: resolve devices, unbind host drivers
  local _usb_vidpids=()
  if [ ${#usb_devices[@]} -gt 0 ]; then
    local dev vidpid port
    for dev in "${usb_devices[@]}"; do
      vidpid="$(_agent_vm_usb_resolve "$dev")" || { _claude_vm_shell_cleanup; trap - EXIT INT TERM; return 1; }
      port="$(_agent_vm_usb_find_sysfs "$vidpid")" || { _claude_vm_shell_cleanup; trap - EXIT INT TERM; return 1; }
      echo "USB: $dev → $vidpid (port $port)"
      _agent_vm_usb_unbind "$port"
      _usb_sysfs_ports+=("$port")
      _usb_vidpids+=("$vidpid")
    done
  fi

  # Start VM — with virtio-balloon if QEMU is available (Linux), plain otherwise
  local _has_balloon=false
  if command -v qemu-system-x86_64 &>/dev/null; then
    # Override memory ceiling if --max-memory was specified
    if [ -n "$max_memory" ]; then
      limactl edit "$vm_name" --memory="$max_memory" --tty=false &>/dev/null
    fi
    if [ ${#_usb_vidpids[@]} -gt 0 ]; then
      _usb_qemu_wrapper="$(_agent_vm_qemu_wrapper "${_usb_vidpids[@]}")"
    else
      _usb_qemu_wrapper="$(_agent_vm_qemu_wrapper)"
    fi
    if QEMU_SYSTEM_X86_64="$_usb_qemu_wrapper" limactl start "$vm_name" &>/dev/null; then
      _has_balloon=true
    else
      # Balloon may have broken QEMU (e.g. PCI conflict); retry without it
      echo "Warning: VM failed to start with balloon, retrying without..." >&2
      limactl stop "$vm_name" &>/dev/null
      rm -f "$_usb_qemu_wrapper"
      if [ ${#_usb_vidpids[@]} -gt 0 ]; then
        _usb_qemu_wrapper="$(_agent_vm_qemu_wrapper --no-balloon "${_usb_vidpids[@]}")"
        QEMU_SYSTEM_X86_64="$_usb_qemu_wrapper" limactl start "$vm_name" &>/dev/null
      else
        _usb_qemu_wrapper=""
        limactl start "$vm_name" &>/dev/null
      fi
    fi

    # Start balloon daemon only if balloon device is present
    if $_has_balloon; then
      local _balloon_script
      _balloon_script="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/balloon-daemon.py"
      local _qmp_sock="$HOME/.lima/$vm_name/qmp.sock"
      local _balloon_daemon_args=()
      if [ -n "$max_memory" ]; then
        _balloon_daemon_args+=(--max-memory "${max_memory}G")
      fi
      python3 "$_balloon_script" "$_qmp_sock" daemon "${_balloon_daemon_args[@]}" &>/dev/null &
      _balloon_daemon_pid=$!
    fi
  else
    if [ -n "$max_memory" ]; then
      echo "Warning: --max-memory ignored without balloon (QEMU not available)" >&2
    fi
    if [ -n "$memory" ]; then
      limactl edit "$vm_name" --memory="$memory" --tty=false &>/dev/null
    fi
    limactl start "$vm_name" &>/dev/null
  fi

  _claude_vm_write_oauth_token "$vm_name" "$_anthropic_token"
  _claude_vm_inject_clipboard_shim "$vm_name" "$state_dir"
  _claude_vm_setup_session_persistence "$vm_name" "$state_dir"
  _claude_vm_ensure_onboarding_config "$vm_name" "$host_dir"

  # Start proxy chain only if there are credential rules
  if [ -n "$_credential_rules" ] && [ "$_credential_rules" != "[]" ]; then
    # Set up mitmproxy (CA cert, system trust, proxy env vars)
    echo "Setting up mitmproxy..."
    _claude_vm_setup_mitmproxy "$vm_name"

    # Build intercepted domains list from credential rules
    local _intercepted_domains
    _intercepted_domains=$(python3 -c "
import json, sys
rules = json.loads(sys.argv[1])
seen = dict.fromkeys(r['domain'] for r in rules)
print(','.join(seen))
" "$_credential_rules")

    _claude_vm_start_mitmproxy "$vm_name" "$_credential_proxy_port" "$_intercepted_domains"
  fi

  if $use_github && [ "$_claude_vm_github_repos_json" != '{}' ]; then
    _claude_vm_inject_git_credentials "$vm_name"
    _claude_vm_inject_gh_credentials "$vm_name"
    _claude_vm_write_instructions "$vm_name" "$_claude_vm_github_repos_json"
  fi

  if [ "$agent" = "opencode" ]; then
    _opencode_vm_setup_config "$vm_name" "$state_dir"
    _opencode_vm_setup_session_persistence "$vm_name" "$state_dir"
    _opencode_vm_setup_auth "$vm_name"
  fi

  # Run project-specific runtime script if it exists
  if [ -f "${host_dir}/.claude-vm.runtime.sh" ]; then
    limactl shell --workdir "$host_dir" "$vm_name" bash -l < "${host_dir}/.claude-vm.runtime.sh"
  fi

  echo "VM: $vm_name | Dir: $host_dir${agent:+ | Agent: $agent}"
  if [ -n "$_credential_proxy_port" ]; then
    echo "Credential proxy: http://host.lima.internal:${_credential_proxy_port}"
    echo "mitmproxy: http://127.0.0.1:8080 (inside VM)"
  fi
  if [ "$agent" = "opencode" ]; then
    echo "OpenCode config: ${state_dir}/opencode-config/opencode.json"
  fi
  echo "Type 'exit' to stop and delete the VM"
  local shell_env=(ENABLE_LSP_TOOL=1)
  if [ "$agent" = "opencode" ]; then
    shell_env+=(OPENCODE_CONFIG="${state_dir}/opencode-config/opencode.json")
  fi
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  CLIPBOARD_DIR="$state_dir" python3 "$script_dir/clipboard-pty.py" \
    limactl shell --workdir "$host_dir" "$vm_name" \
    env "${shell_env[@]}" \
    bash -l
  _claude_vm_security_check "$host_dir" "$security_snapshot"

  _claude_vm_shell_cleanup
  trap - EXIT INT TERM
}

# ── Balloon memory control ────────────────────────────────────────────────────

_agent_vm_memory() {
  if ! command -v qemu-system-x86_64 &>/dev/null; then
    echo "Error: 'agent-vm memory' requires QEMU (balloon not available on this platform)" >&2
    return 1
  fi

  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

  if [ $# -eq 0 ]; then
    # List running VMs with current balloon size
    local vm found=false
    while IFS= read -r vm; do
      [ -z "$vm" ] && continue
      [ "$vm" = "$CLAUDE_VM_TEMPLATE" ] && continue
      local sock="$HOME/.lima/$vm/qmp.sock"
      if [ -S "$sock" ]; then
        local size
        size=$(python3 "$script_dir/balloon-daemon.py" "$sock" get 2>/dev/null) || size="?"
        echo "$vm: $size"
        found=true
      fi
    done < <(limactl list -q --status Running 2>/dev/null)
    if ! $found; then
      echo "No running VMs found." >&2
    fi
    return
  fi

  local vm_name="" target=""
  # If first arg looks like a size (e.g. 8G, 4096M), auto-detect the running VM
  if [[ "$1" =~ ^[0-9]+[GMgm]?$ ]]; then
    local vms=()
    while IFS= read -r vm; do
      [ -z "$vm" ] && continue
      [ "$vm" = "$CLAUDE_VM_TEMPLATE" ] && continue
      vms+=("$vm")
    done < <(limactl list -q --status Running 2>/dev/null)
    if [ ${#vms[@]} -eq 0 ]; then
      echo "Error: no running VMs found" >&2
      return 1
    elif [ ${#vms[@]} -gt 1 ]; then
      echo "Error: multiple VMs running, specify name:" >&2
      printf '  %s\n' "${vms[@]}" >&2
      return 1
    fi
    vm_name="${vms[0]}"
    target="$1"
  else
    vm_name="$1"
    target="${2:-}"
  fi

  local qmp_sock="$HOME/.lima/$vm_name/qmp.sock"
  if [ ! -S "$qmp_sock" ]; then
    echo "Error: VM '$vm_name' not running (no QMP socket)" >&2
    return 1
  fi

  if [ -z "$target" ]; then
    python3 "$script_dir/balloon-daemon.py" "$qmp_sock" get
  else
    python3 "$script_dir/balloon-daemon.py" "$qmp_sock" set "$target"
  fi
}

# ── Main entry point ──────────────────────────────────────────────────────────

agent-vm() {
  local subcmd="${1:-}"
  if [ -z "$subcmd" ] || [ "$subcmd" = "--help" ] || [ "$subcmd" = "-h" ]; then
    echo "Usage: agent-vm <command> [options]"
    echo ""
    echo "Run AI coding agents inside sandboxed Lima VMs."
    echo ""
    echo "Commands:"
    echo "  setup              Create the VM template (run once)"
    echo "  claude [args]      Run Claude Code in a sandboxed VM"
    echo "  opencode [args]    Run OpenCode in a sandboxed VM"
    echo "  shell [agent]      Open a debug shell (optionally pre-configured for an agent)"
    echo "  memory [vm] [size] Query or adjust VM memory via balloon (e.g. 'memory 12G')"
    echo ""
    echo "Options (for claude, opencode, shell):"
    echo "  --memory GB        Initial memory for the VM (default: 2G with balloon, 4G without)"
    echo "  --max-memory GB    Memory ceiling (default: from template; balloon grows up to this)"
    echo "  --no-git           Skip GitHub integration"
    echo "  --usb DEVICE       Pass USB device to VM (repeatable; /dev/ttyACM0 or 1a86:55d3)"
    echo ""
    echo "Memory management:"
    echo "  On Linux (QEMU), VMs use a virtio-balloon to start with 2G and auto-grow"
    echo "  up to the ceiling as the guest needs more memory."
    echo "  Example: agent-vm claude --memory 5 --max-memory 10"
    echo "  On macOS (VZ), --memory sets a fixed allocation (default: 4G, no balloon)."
    echo ""
    echo "Run 'agent-vm <command> --help' for command-specific help."
    [ -z "$subcmd" ] && return 1
    return 0
  fi
  shift

  case "$subcmd" in
    setup)
      _agent_vm_setup "$@"
      ;;
    claude)
      _agent_vm_run claude "$@"
      ;;
    opencode)
      _agent_vm_run opencode "$@"
      ;;
    shell)
      _agent_vm_shell "$@"
      ;;
    memory)
      _agent_vm_memory "$@"
      ;;
    *)
      echo "Error: Unknown command '$subcmd'" >&2
      echo "Run 'agent-vm' for usage." >&2
      return 1
      ;;
  esac
}
