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
#   agent-vm codex [args]     - Run Codex in a fresh VM (args forwarded to codex)
#   agent-vm copilot [args]   - Run GitHub Copilot CLI in a fresh VM (args forwarded to copilot)
#   agent-vm shell [agent]    - Open a debug shell in a fresh VM

CLAUDE_VM_TEMPLATE="claude-template"

# Capture script directory at source time — BASH_SOURCE[0] is only reliable
# at the top level in zsh; inside functions it may resolve to empty/cwd.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"

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
        echo "Create a VM template with Claude Code, OpenCode, Codex, and Copilot CLI pre-installed."
        echo ""
        echo "Options:"
        echo "  --minimal      Only install git, curl, jq, Claude Code, OpenCode, Codex, and Copilot CLI"
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
    git curl jq sshfs gh ca-certificates
  _claude_vm_install_host_proxy_ca "$CLAUDE_VM_TEMPLATE"

  if ! $minimal; then
    limactl shell "$CLAUDE_VM_TEMPLATE" sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
      wget build-essential \
      python3 python3-pip python3-venv \
      ripgrep fd-find htop \
      unzip zip \
      ca-certificates \
      qemu-user-static binfmt-support \
      mosquitto-clients

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

  # Install Codex
  echo "Installing Codex..."
  limactl shell "$CLAUDE_VM_TEMPLATE" bash -lc 'curl -fsSL https://github.com/openai/codex/releases/latest/download/install.sh | sh'

  # Install GitHub Copilot CLI
  echo "Installing GitHub Copilot CLI..."
  limactl shell "$CLAUDE_VM_TEMPLATE" bash -lc 'curl -fsSL https://gh.io/copilot-install | bash'

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

  echo "Template ready. Run 'agent-vm claude', 'agent-vm opencode', or 'agent-vm codex' in any project directory."
}

_claude_vm_install_host_proxy_ca() {
  local vm_name="$1"
  local host_ca="$HOME/.mitmproxy/mitmproxy-ca-cert.pem"
  if [ ! -f "$host_ca" ]; then
    return
  fi

  echo "Installing host MITM CA into VM trust store..."
  cat "$host_ca" | limactl shell "$vm_name" sudo tee /usr/local/share/ca-certificates/host-mitmproxy-ca.crt > /dev/null
  limactl shell "$vm_name" sudo update-ca-certificates
  limactl shell "$vm_name" git config --global http.sslCAInfo /etc/ssl/certs/ca-certificates.crt
}

_claude_vm_build_credential_rules() {
  # Build CREDENTIAL_PROXY_RULES JSON from tokens.
  # Args: openai_token github_repos_json copilot_token host_claude_credentials_file
  # github_repos_json: '{"owner/repo": "token", ...}' — per-repo tokens
  # host_claude_credentials_file: if set, the proxy reads the Anthropic
  #   Authorization header from this host file on each request and intercepts
  #   platform.claude.com OAuth refresh by letting host Claude rotate tokens.
  local openai_token="$1"
  local github_repos_json="$2"
  local copilot_token="$3"
  local host_claude_credentials_file="${4:-}"

  python3 -c "
import base64, json, sys
rules = []
openai_token = sys.argv[1]
repos = json.loads(sys.argv[2]) if sys.argv[2] else {}
copilot_token = sys.argv[3]
host_claude_creds = sys.argv[4]
PLACEHOLDER_ACCESS = '$CLAUDE_VM_PLACEHOLDER_ACCESS_TOKEN'
PLACEHOLDER_REFRESH = '$CLAUDE_VM_PLACEHOLDER_REFRESH_TOKEN'
# Always intercept api.anthropic.com so traffic can be upstream-proxied.
# If host creds are available, also intercept platform.claude.com's OAuth
# refresh endpoint — see doc/architecture.md ADR-7.
anthropic_rule = {'domain': 'api.anthropic.com', 'headers': {}, 'use_proxy': True}
if host_claude_creds:
    anthropic_rule['auth_from_file'] = {
        'path': host_claude_creds,
        'json_path': 'claudeAiOauth.accessToken',
        'header': 'Authorization',
        'format': 'Bearer {token}',
    }
rules.append(anthropic_rule)
if host_claude_creds:
    rules.append({
        'domain': 'platform.claude.com',
        'path_prefix': '/v1/oauth/token',
        'headers': {},
        'oauth_refresh': {
            'credentials_file': host_claude_creds,
            'json_path': 'claudeAiOauth',
            'placeholder_access_token': PLACEHOLDER_ACCESS,
            'placeholder_refresh_token': PLACEHOLDER_REFRESH,
        },
    })
openai_headers = {}
if openai_token:
    openai_headers['Authorization'] = f'Bearer {openai_token}'
rules.append({
    'domain': 'api.openai.com',
    'headers': openai_headers,
    'use_proxy': True,
})
rules.append({
    'domain': 'chatgpt.com',
    'headers': openai_headers,
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
if copilot_token:
    if copilot_token == 'proxy-managed':
        # Upstream proxy handles auth — just route through it
        for domain in ('api.githubcopilot.com', 'api.individual.githubcopilot.com'):
            rules.append({
                'domain': domain,
                'headers': {},
                'use_proxy': True,
            })
    else:
        copilot_headers = {'Authorization': f'Bearer {copilot_token}'}
        for domain in ('api.githubcopilot.com', 'api.individual.githubcopilot.com'):
            rules.append({
                'domain': domain,
                'headers': copilot_headers,
            })
print(json.dumps(rules))
" "$openai_token" "$github_repos_json" "$copilot_token" "$host_claude_credentials_file"
}

_claude_vm_start_credential_proxy() {
  local rules_json="$1"

  # Generate per-instance secret for cross-VM isolation
  _credential_proxy_secret=$(python3 -c "import secrets; print(secrets.token_hex(32))")

  echo "Starting credential proxy..."
  exec 3< <(CREDENTIAL_PROXY_RULES="$rules_json" \
    CREDENTIAL_PROXY_SECRET="$_credential_proxy_secret" \
    CREDENTIAL_PROXY_DEBUG="${CREDENTIAL_PROXY_DEBUG:-0}" \
    CREDENTIAL_PROXY_LOG_DIR="${CREDENTIAL_PROXY_LOG_DIR:-.}" \
    python3 "$SCRIPT_DIR/credential-proxy.py")
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
  # Copy addon script to VM
  limactl shell "$vm_name" bash -c 'mkdir -p ~/.mitmproxy'
  cat "$SCRIPT_DIR/mitmproxy-addon.py" | limactl shell "$vm_name" bash -c 'cat > ~/.mitmproxy/addon.py'

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

_codex_vm_host_auth_file() {
  local codex_home="${CODEX_HOME:-$HOME/.codex}"
  printf '%s\n' "${codex_home%/}/auth.json"
}

_codex_vm_refresh_host_auth() {
  if ! command -v codex >/dev/null 2>&1; then
    return 1
  fi
  local tmpdir
  tmpdir="$(mktemp -d)"
  if codex exec \
    --skip-git-repo-check \
    --dangerously-bypass-approvals-and-sandbox \
    --color never \
    -C "$tmpdir" \
    "Reply with exactly OK and nothing else." >/dev/null 2>&1; then
    rm -rf "$tmpdir"
    return 0
  fi
  rm -rf "$tmpdir"
  return 1
}

_codex_vm_read_valid_host_auth() {
  local auth_file
  auth_file="$(_codex_vm_host_auth_file)"
  [ -f "$auth_file" ] || return 1
  python3 -c '
import base64, json, pathlib, sys

auth = json.loads(pathlib.Path(sys.argv[1]).read_text())
if auth.get("auth_mode") != "chatgpt":
    sys.exit(1)
tokens = auth.get("tokens") or {}
required = ("id_token", "access_token", "refresh_token", "account_id")
if not all(tokens.get(k) for k in required):
    sys.exit(1)

def decode_payload(jwt):
    parts = jwt.split(".")
    if len(parts) != 3:
        raise ValueError("invalid jwt")
    payload = parts[1] + "=" * (-len(parts[1]) % 4)
    return json.loads(base64.urlsafe_b64decode(payload))

try:
    id_claims = decode_payload(tokens["id_token"])
    access_claims = decode_payload(tokens["access_token"])
except Exception:
    sys.exit(1)

auth_claims = id_claims.get("https://api.openai.com/auth") or {}
account_id = (
    tokens.get("account_id")
    or auth_claims.get("chatgpt_account_id")
    or (access_claims.get("https://api.openai.com/auth") or {}).get("chatgpt_account_id")
)
plan_type = (
    auth_claims.get("chatgpt_plan_type")
    or (access_claims.get("https://api.openai.com/auth") or {}).get("chatgpt_plan_type")
)
email = (
    id_claims.get("email")
    or (id_claims.get("https://api.openai.com/profile") or {}).get("email")
)
exp = access_claims.get("exp")
if not account_id or not isinstance(exp, int):
    sys.exit(1)

print(json.dumps({
    "access_token": tokens["access_token"],
    "account_id": account_id,
    "plan_type": plan_type,
    "email": email,
    "access_exp": exp,
    "last_refresh": auth.get("last_refresh"),
}))
' "$auth_file"
}

_codex_vm_build_placeholder_auth_json() {
  local host_auth_json="$1"
  python3 -c '
import base64, json, sys

host = json.loads(sys.argv[1])

def encode(payload):
    header = {"alg": "RS256", "typ": "JWT"}
    h = base64.urlsafe_b64encode(json.dumps(header, separators=(",", ":")).encode()).decode().rstrip("=")
    p = base64.urlsafe_b64encode(json.dumps(payload, separators=(",", ":")).encode()).decode().rstrip("=")
    return f"{h}.{p}.placeholder"

account_id = host["account_id"]
plan_type = host.get("plan_type")
email = host.get("email")
exp = int(host["access_exp"])
iat = max(0, exp - 3600)

id_payload = {
    "iss": "https://auth.openai.com",
    "aud": ["app_EMoamEEZ73f0CkXaXp7hrann"],
    "iat": iat,
    "exp": exp,
    "https://api.openai.com/auth": {
        "chatgpt_account_id": account_id,
        "chatgpt_plan_type": plan_type,
    },
}
if email:
    id_payload["email"] = email
access_payload = {
    "iss": "https://auth.openai.com",
    "aud": ["https://api.openai.com/v1"],
    "iat": iat,
    "nbf": iat,
    "exp": exp,
    "scp": ["openid", "profile", "email", "offline_access"],
    "https://api.openai.com/auth": {
        "chatgpt_account_id": account_id,
        "chatgpt_plan_type": plan_type,
    },
}

placeholder = {
    "auth_mode": "chatgpt",
    "OPENAI_API_KEY": None,
    "tokens": {
        "id_token": encode(id_payload),
        "access_token": encode(access_payload),
        "refresh_token": "placeholder-refresh-token-injected-by-proxy",
        "account_id": account_id,
    },
    "last_refresh": host.get("last_refresh"),
}
print(json.dumps(placeholder))
' "$host_auth_json"
}

_codex_vm_prepare_host_auth() {
  local host_auth_json
  if ! host_auth_json="$(_codex_vm_read_valid_host_auth)"; then
    return 1
  fi
  if ! _codex_vm_refresh_host_auth; then
    return 1
  fi
  host_auth_json="$(_codex_vm_read_valid_host_auth)" || return 1
  _codex_host_auth_json="$host_auth_json"
  _codex_proxy_token="$(python3 -c 'import json, sys; print(json.loads(sys.argv[1])["access_token"])' "$host_auth_json")"
  _codex_placeholder_auth_json="$(_codex_vm_build_placeholder_auth_json "$host_auth_json")" || return 1
}

_opencode_vm_build_oauth_auth_json() {
  local host_auth_json="$1"
  python3 -c '
import base64, json, sys

host = json.loads(sys.argv[1])

def encode(payload):
    header = {"alg": "RS256", "typ": "JWT"}
    h = base64.urlsafe_b64encode(json.dumps(header, separators=(",", ":")).encode()).decode().rstrip("=")
    p = base64.urlsafe_b64encode(json.dumps(payload, separators=(",", ":")).encode()).decode().rstrip("=")
    return f"{h}.{p}.placeholder"

account_id = host["account_id"]
plan_type = host.get("plan_type")
email = host.get("email")
exp = int(host["access_exp"])
iat = max(0, exp - 3600)

access_payload = {
    "iss": "https://auth.openai.com",
    "aud": ["https://api.openai.com/v1"],
    "iat": iat,
    "nbf": iat,
    "exp": exp,
    "scp": ["openid", "profile", "email", "offline_access"],
    "https://api.openai.com/auth": {
        "chatgpt_account_id": account_id,
        "chatgpt_plan_type": plan_type,
    },
}
if email:
    access_payload["email"] = email

print(json.dumps({
    "type": "oauth",
    "refresh": "placeholder-refresh-token-injected-by-proxy",
    "access": encode(access_payload),
    "expires": exp * 1000,
    "accountId": account_id,
}))
' "$host_auth_json"
}

_opencode_vm_prepare_host_auth() {
  local host_auth_json
  if ! host_auth_json="$(_codex_vm_read_valid_host_auth)"; then
    return 1
  fi
  if ! _codex_vm_refresh_host_auth; then
    return 1
  fi
  host_auth_json="$(_codex_vm_read_valid_host_auth)" || return 1
  _opencode_host_auth_json="$host_auth_json"
  _opencode_openai_auth_json="$(_opencode_vm_build_oauth_auth_json "$host_auth_json")" || return 1
}

_claude_vm_post_boot_setup() {
  # Shared post-boot setup for both claude-vm and claude-vm-shell.
  # Uses variables from the calling function's scope:
  #   vm_name, host_dir, state_dir, agent, use_github,
  #   _host_claude_credentials_file, _openai_token, _codex_proxy_token, _copilot_token,
  #   _codex_placeholder_auth_json, _credential_rules, _credential_proxy_port,
  #   _credential_proxy_secret, _claude_vm_github_repos_json
  # Sets: _oauth_token, _codex_home, _intercepted_domains

  _claude_vm_inject_clipboard_shim "$vm_name" "$state_dir"
  _oauth_token=""
  _codex_home=""

  if [ "$agent" = "claude" ] || [ "$agent" = "opencode" ] || [ -z "$agent" ]; then
    # Either mirror the host credentials file (proxy MITMs refresh) or fall
    # back to a placeholder env var so Claude still makes API calls via
    # AI_HTTPS_PROXY. Real tokens never enter the VM either way.
    if [ -n "${_host_claude_credentials_file:-}" ]; then
      _claude_vm_write_placeholder_credentials "$vm_name" "$_host_claude_credentials_file"
    else
      _oauth_token="${AI_HTTPS_PROXY:+placeholder}"
      _claude_vm_write_oauth_token "$vm_name" "$_oauth_token"
    fi
    _claude_vm_setup_session_persistence "$vm_name" "$state_dir"
    _claude_vm_ensure_onboarding_config "$vm_name" "$host_dir"
  fi

  if [ "$agent" = "codex" ] || [ -z "$agent" ]; then
    _codex_vm_setup_home "$vm_name" "$state_dir" "${_codex_placeholder_auth_json:-}"
    _codex_vm_write_api_key "$vm_name" "${_openai_token:-}"
    _codex_home="${state_dir}/codex-home"
  fi

  if [ "$agent" = "copilot" ] || [ -z "$agent" ]; then
    _copilot_vm_setup_home "$vm_name" "$state_dir"
  fi

  _copilot_vm_write_token "$vm_name" "${_copilot_token:-}"

  # Start proxy chain only if there are credential rules
  _intercepted_domains=""
  if [ -n "$_credential_rules" ] && [ "$_credential_rules" != "[]" ]; then
    echo "Setting up mitmproxy..."
    _claude_vm_setup_mitmproxy "$vm_name"

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

    # Codex does not honor ~/.claude/CLAUDE.md; it reads ~/.codex/AGENTS.md
    if [ "$agent" = "codex" ] || [ -z "$agent" ]; then
      limactl shell "$vm_name" bash -c \
        'ln -sf "$HOME/.claude/CLAUDE.md" "$HOME/.codex/AGENTS.md"'
    fi

    # Copilot CLI reads ~/.copilot/copilot-instructions.md
    if [ "$agent" = "copilot" ] || [ -z "$agent" ]; then
      limactl shell "$vm_name" bash -c \
        'mkdir -p "$HOME/.copilot" && ln -sf "$HOME/.claude/CLAUDE.md" "$HOME/.copilot/copilot-instructions.md"'
    fi
  fi

  if [ "$agent" = "opencode" ] || [ -z "$agent" ]; then
    _opencode_vm_setup_config "$vm_name" "$state_dir"
    _opencode_vm_setup_session_persistence "$vm_name" "$state_dir"
    _opencode_vm_setup_auth "$vm_name"
  fi

  # Run project-specific runtime script if it exists
  if [ -f "${host_dir}/.claude-vm.runtime.sh" ]; then
    echo "Running project runtime setup..."
    limactl shell --workdir "$host_dir" "$vm_name" bash -l < "${host_dir}/.claude-vm.runtime.sh"
  fi
}

_claude_vm_print_config() {
  # Print proxy/credential configuration summary.
  # Uses variables from the calling function's scope.
  if [ -n "$_credential_proxy_port" ]; then
    echo "Credential proxy: http://host.lima.internal:${_credential_proxy_port}"
    echo "Intercepted domains: $_intercepted_domains"
  fi
  if [ -n "$_oauth_token" ]; then
    local _truncated="${_oauth_token:0:16}"
    [ ${#_oauth_token} -gt 16 ] && _truncated="${_truncated}..."
    echo "VM env: CLAUDE_CODE_OAUTH_TOKEN=${_truncated}"
  fi
  if [ -n "${_host_claude_credentials_file:-}" ]; then
    echo "VM auth: placeholder ~/.claude/.credentials.json; host file rotates tokens on refresh"
  fi
  if [ -n "${_openai_token:-}" ]; then
    echo "VM env: OPENAI_API_KEY=dummy-key-auth-handled-by-proxy"
  fi
  if [ -n "${_copilot_token:-}" ]; then
    echo "VM auth: github-copilot placeholder in auth.json (proxy injects gho_* token at runtime)"
  fi
  if [ -n "${_codex_proxy_token:-}" ] && [ -n "${_codex_placeholder_auth_json:-}" ]; then
    echo "VM auth: ~/.codex/auth.json placeholders backed by host Codex auth"
  fi
  if [ -n "${_codex_home:-}" ]; then
    echo "Codex home: ${_codex_home}"
  fi
  if [ -n "${AI_HTTPS_PROXY:-}" ]; then
    echo "AI upstream proxy: $AI_HTTPS_PROXY"
  fi
  if [ -n "${AI_SSL_CERT_FILE:-}" ]; then
    echo "AI SSL cert file: $AI_SSL_CERT_FILE"
  fi
}

_claude_vm_write_oauth_token() {
  local vm_name="$1"
  local token="$2"
  if [ -z "$token" ]; then
    return
  fi
  # Set CLAUDE_CODE_OAUTH_TOKEN so Claude Code attempts API requests.
  # The value is always a placeholder; the real auth header is injected
  # by the host credential proxy.
  limactl shell "$vm_name" bash -c "
    sudo tee /etc/profile.d/claude-oauth.sh > /dev/null <<EOF
export CLAUDE_CODE_OAUTH_TOKEN=$token
EOF
    sudo chmod 644 /etc/profile.d/claude-oauth.sh
  "
}

# Placeholder tokens written into the VM's ~/.claude/.credentials.json.
# The credential proxy rewrites them on the wire, so they never need to be real.
CLAUDE_VM_PLACEHOLDER_ACCESS_TOKEN="sk-ant-oat01-placeholder-proxy-managed"
CLAUDE_VM_PLACEHOLDER_REFRESH_TOKEN="sk-ant-ort01-placeholder-proxy-managed"

_claude_vm_detect_host_claude_credentials() {
  # Print the host's ~/.claude/.credentials.json path to stdout if it exists
  # and has a complete claudeAiOauth block; announce on stderr. Empty stdout
  # means "no host creds" and the caller should fall back to AI_HTTPS_PROXY
  # (or run without Anthropic auth).
  local path="$HOME/.claude/.credentials.json"
  [ -r "$path" ] || return 0
  if python3 -c "
import json, sys
try:
    with open(sys.argv[1]) as f:
        data = json.load(f)
    oauth = data.get('claudeAiOauth') or {}
    if not (oauth.get('accessToken') and oauth.get('refreshToken') and oauth.get('expiresAt')):
        sys.exit(1)
except Exception:
    sys.exit(1)
" "$path" 2>/dev/null; then
    echo "$path"
    echo "Using host Claude credentials via credential proxy MITM (token rotation stays on host)." >&2
  fi
}

_claude_vm_write_placeholder_credentials() {
  # Write a placeholder ~/.claude/.credentials.json into the VM. Claude Code
  # will read expiresAt to decide when to refresh, so we mirror the host value;
  # everything else is a fixed placeholder that the credential proxy rewrites.
  local vm_name="$1"
  local host_creds="$2"
  local payload
  payload=$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    host = json.load(f)
ho = host.get('claudeAiOauth') or {}
out = {
  'claudeAiOauth': {
    'accessToken': sys.argv[2],
    'refreshToken': sys.argv[3],
    'expiresAt': ho.get('expiresAt', 0),
    'scopes': ho.get('scopes', []),
  }
}
for k in ('subscriptionType', 'rateLimitTier'):
    if k in ho:
        out['claudeAiOauth'][k] = ho[k]
print(json.dumps(out))
" "$host_creds" "$CLAUDE_VM_PLACEHOLDER_ACCESS_TOKEN" "$CLAUDE_VM_PLACEHOLDER_REFRESH_TOKEN") || return 1
  limactl shell "$vm_name" bash -c "
    mkdir -p \$HOME/.claude
    cat > \$HOME/.claude/.credentials.json <<'CREDS'
$payload
CREDS
    chmod 600 \$HOME/.claude/.credentials.json
  "
}

_codex_vm_write_api_key() {
  local vm_name="$1"
  local token="$2"
  if [ -z "$token" ]; then
    return
  fi
  # Use a placeholder API key inside the VM. Real auth is injected by the
  # host credential proxy for api.openai.com.
  limactl shell "$vm_name" bash -c "
    sudo tee /etc/profile.d/codex-api-key.sh > /dev/null <<EOF
export OPENAI_API_KEY=dummy-key-auth-handled-by-proxy
EOF
    sudo chmod 644 /etc/profile.d/codex-api-key.sh
  "
}

_copilot_vm_write_token() {
  local vm_name="$1"
  local token="$2"
  if [ -z "$token" ]; then
    return
  fi
  # Use a placeholder token inside the VM. Real auth is injected by the
  # host credential proxy for api.githubcopilot.com.
  limactl shell "$vm_name" bash -c "
    sudo tee /etc/profile.d/copilot-token.sh > /dev/null <<EOF
export COPILOT_GITHUB_TOKEN=placeholder-copilot-token-injected-by-proxy
EOF
    sudo chmod 644 /etc/profile.d/copilot-token.sh
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

_codex_vm_setup_home() {
  local vm_name="$1"
  local state_dir="$2"
  local placeholder_auth_json="${3:-}"
  limactl shell "$vm_name" bash -c '
    CODEX_HOME_DIR="'"$state_dir"'/codex-home"
    mkdir -p "$CODEX_HOME_DIR"
    if [ -d ~/.codex ] && [ ! -L ~/.codex ]; then
      cp -a ~/.codex/. "$CODEX_HOME_DIR/" 2>/dev/null || true
      rm -rf ~/.codex
    fi
    ln -sfn "$CODEX_HOME_DIR" ~/.codex
    if [ -f "$CODEX_HOME_DIR/auth.json" ] && grep -q "placeholder-refresh-token-injected-by-proxy\|\\.placeholder\"" "$CODEX_HOME_DIR/auth.json"; then
      rm -f "$CODEX_HOME_DIR/auth.json"
    fi
    if [ ! -f "$CODEX_HOME_DIR/config.toml" ]; then
      cat > "$CODEX_HOME_DIR/config.toml" << '\''CONFIG'\''
sandbox_mode = "danger-full-access"
approval_policy = "never"
CONFIG
    fi
  '
  if [ -n "$placeholder_auth_json" ]; then
    limactl shell "$vm_name" bash -c "
      cat > '$state_dir/codex-home/auth.json' <<'AUTH'
$placeholder_auth_json
AUTH
      chmod 600 '$state_dir/codex-home/auth.json'
    "
  fi
}

_copilot_vm_setup_home() {
  local vm_name="$1"
  local state_dir="$2"
  limactl shell "$vm_name" bash -c '
    COPILOT_HOME_DIR="'"$state_dir"'/copilot-home"
    mkdir -p "$COPILOT_HOME_DIR"
    if [ -d ~/.copilot ] && [ ! -L ~/.copilot ]; then
      cp -a ~/.copilot/. "$COPILOT_HOME_DIR/" 2>/dev/null || true
      rm -rf ~/.copilot
    fi
    ln -sfn "$COPILOT_HOME_DIR" ~/.copilot
    # Trust all folders — the VM itself is the sandbox
    CONFIG="$COPILOT_HOME_DIR/config.json"
    if [ ! -f "$CONFIG" ]; then
      echo "{}" > "$CONFIG"
    fi
    jq ".trusted_folders = [\"/\"]" "$CONFIG" > "$CONFIG.tmp" && mv "$CONFIG.tmp" "$CONFIG"
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

  # Push to a fully-qualified probe ref so detached HEAD still works.
  # --dry-run means nothing is actually created on the remote.
  local push_output
  push_output=$(git -C "$host_dir" push --dry-run --no-verify origin HEAD:refs/heads/__claude_vm_probe__ 2>&1) && return 0
  # Server rejected the ref (not the auth) = have push access
  echo "$push_output" | grep -qiE 'non-fast-forward|up to date|Everything up-to-date|fetch first|\[rejected\]' && return 0
  return 1
}

_claude_vm_get_github_token() {
  # Acquire a GitHub token for the project, its submodules, and any extra
  # mounted directories that are GitHub repos.
  # Usage: _claude_vm_get_github_token host_dir [extra_mount ...]
  # Sets _claude_vm_github_token (first repo's token) and _claude_vm_github_repos_json.
  local host_dir="$1"
  shift
  local extra_mounts=("$@")
  local -A _seen_repos=()  # slug -> token, for dedup and final JSON build
  _claude_vm_github_token=""

  _get_token_for_repo() {
    local _owner="$1" _repo="$2" _url="$3" _dir="$4"
    local _slug="$_owner/$_repo"
    [[ -n "${_seen_repos[$_slug]+x}" ]] && return 0
    if ! _claude_vm_check_push_access "$_dir"; then
      echo "  $_slug: no write access, skipping token request"
      return 0
    fi
    echo "Requesting GitHub token for $_slug..."
    local _token
    _token=$(python3 "$SCRIPT_DIR/github_app_token_demo.py" \
      user-token --client-id Iv23liisR1WdpJmDUPLT \
      --repo "$_url" --token-only \
      --cache-dir "$HOME/.cache/claude-vm") || true
    if [ -n "$_token" ]; then
      _seen_repos[$_slug]="$_token"
      [ -z "$_claude_vm_github_token" ] && _claude_vm_github_token="$_token"
      echo "  Got token for $_slug"
    else
      echo "  Warning: no token for $_slug"
    fi
  }

  _scan_dir_for_repos() {
    local _dir="$1"
    local _label="${2:-}"
    [ -d "$_dir/.git" ] || [ -f "$_dir/.git" ] || return 0

    local _url _owner _repo
    _url=$(git -C "$_dir" remote get-url origin 2>/dev/null) || true
    if [ -n "$_url" ]; then
      read -r _owner _repo < <(_claude_vm_parse_github_remote "$_url") || true
      if [ -n "$_owner" ] && [ -n "$_repo" ]; then
        if [ -n "$_label" ]; then
          echo "Found GitHub repo $_owner/$_repo in ${_label} $(basename "$_dir")"
        fi
        _get_token_for_repo "$_owner" "$_repo" "$_url" "$_dir"
      fi
    fi

    if [ -f "$_dir/.gitmodules" ]; then
      local _sub_entries
      _sub_entries=$(python3 -c "
import configparser, sys
p = configparser.ConfigParser()
p.read(sys.argv[1])
for s in p.sections():
    url = p.get(s, 'url', fallback='')
    path = p.get(s, 'path', fallback='')
    if 'github.com' in url:
        print(f'{url}\t{path}')
" "$_dir/.gitmodules")
      local _sub_url _sub_path _sub_owner _sub_repo _sub_dir
      while IFS=$'\t' read -r _sub_url _sub_path; do
        [ -z "$_sub_url" ] && continue
        read -r _sub_owner _sub_repo < <(_claude_vm_parse_github_remote "$_sub_url") || continue
        [ -z "$_sub_owner" ] || [ -z "$_sub_repo" ] && continue

        _sub_dir=""
        [ -n "$_sub_path" ] && [ -e "$_dir/$_sub_path/.git" ] && _sub_dir="$_dir/$_sub_path"
        _get_token_for_repo "$_sub_owner" "$_sub_repo" "$_sub_url" "$_sub_dir"
      done <<< "$_sub_entries"
    fi
  }

  _scan_dir_for_repos "$host_dir"

  local _host_realpath
  _host_realpath="$(realpath "$host_dir" 2>/dev/null)" || true
  local _mount
  for _mount in "${extra_mounts[@]}"; do
    _mount="$(realpath "$_mount" 2>/dev/null)" || continue
    [ "$_mount" = "$_host_realpath" ] && continue
    _scan_dir_for_repos "$_mount" "mount"
  done

  if [ ${#_seen_repos[@]} -eq 0 ]; then
    echo "Warning: No GitHub repos found (no remote, no submodules, no mounts), skipping GitHub" >&2
    _claude_vm_github_repos_json='{}'
    return 1
  fi

  # Build repos JSON from associative array
  local _repos_json="{"
  local _first=true _slug
  for _slug in "${!_seen_repos[@]}"; do
    $_first || _repos_json="${_repos_json},"
    _first=false
    _repos_json="${_repos_json}\"${_slug}\":\"${_seen_repos[$_slug]}\""
  done
  _repos_json="${_repos_json}}"

  echo "GitHub token acquired (scope: $(IFS=', '; echo "${!_seen_repos[*]}"))"
  _claude_vm_github_repos_json="$_repos_json"
}


_claude_vm_get_copilot_token() {
  # Populate _copilot_token for use with api.githubcopilot.com.
  # Uses the OpenCode OAuth App (Ov23li8tweQw6odWQebz, read:user scope) —
  # the only app that grants full model access (Claude, Gemini, GPT-5, etc.).

  # Skip device flow when upstream proxy handles Copilot auth
  if [ "${COPILOT_SKIP_DEVICE_FLOW:-0}" = "1" ] || [ -n "${AI_HTTPS_PROXY:-}" ]; then
    echo "  Copilot auth handled by upstream proxy, skipping device flow"
    _copilot_token="proxy-managed"
    return
  fi

  local copilot_cache_file="$HOME/.cache/claude-vm/copilot-token.json"
  local token
  token=$(python3 "$SCRIPT_DIR/copilot_token.py" "$copilot_cache_file") || true
  if [ -n "$token" ]; then
    _copilot_token="$token"
  else
    echo "  Warning: could not obtain Copilot token, Copilot API will be unavailable"
  fi
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

  python3 -c "
import json
config = {
    '\$schema': 'https://opencode.ai/config.json',
    'provider': {
        'anthropic': {
            'options': {
                'baseURL': 'https://api.anthropic.com/v1',
                'apiKey': 'dummy-key-auth-handled-by-proxy'
            }
        }
    },
    'permission': 'allow',
    'autoupdate': False,
    'watcher': {
        'ignore': ['node_modules/**', 'dist/**', '.git/**', '.agent-vm/**']
    },
    'mcp': {
        'chrome-devtools': {
            'type': 'local',
            'command': ['npx', '-y', 'chrome-devtools-mcp@latest', '--headless=true', '--isolated=true'],
            'enabled': True
        }
    }
}
print(json.dumps(config, indent=2))
" > "${config_dir}/opencode.json"
}

_opencode_vm_setup_auth() {
  local vm_name="$1"
  # Write a valid OpenCode auth.json. For OpenAI, prefer native OAuth auth when
  # host Codex ChatGPT auth is available; otherwise fall back to the proxy-based
  # API-key flow.
  local _anthropic_auth_json=""
  local _openai_api_auth_json=""
  local _openai_oauth_auth_json="${_opencode_openai_auth_json:-}"
  if [ -n "${_host_claude_credentials_file:-}" ]; then
    _anthropic_auth_json='{"type":"api","key":"dummy-key-auth-handled-by-proxy"}'
  fi
  if [ -z "$_openai_oauth_auth_json" ] && { [ -n "${_openai_token:-}" ] || [ -n "${_codex_proxy_token:-}" ]; }; then
    _openai_api_auth_json='{"type":"api","key":"dummy-key-auth-handled-by-proxy"}'
  fi

  local _copilot_auth_json=""
  if [ -n "${_copilot_token:-}" ]; then
    _copilot_auth_json='{"type":"oauth","refresh":"placeholder-copilot-token-injected-by-proxy","access":"placeholder-copilot-token-injected-by-proxy","expires":0}'
  fi

  local auth_json
  auth_json=$(python3 -c "
import json, sys
anthropic = sys.argv[1]
openai_api = sys.argv[2]
openai_oauth = sys.argv[3]
copilot = sys.argv[4]
auth = {}
if anthropic:
    auth['anthropic'] = json.loads(anthropic)
if openai_oauth:
    auth['openai'] = json.loads(openai_oauth)
elif openai_api:
    auth['openai'] = json.loads(openai_api)
if copilot:
    auth['github-copilot'] = json.loads(copilot)
print(json.dumps(auth, indent=2))
" "$_anthropic_auth_json" "$_openai_api_auth_json" "$_openai_oauth_auth_json" "$_copilot_auth_json")

  limactl shell "$vm_name" bash -c '
    mkdir -p ~/.local/share/opencode
    cat > ~/.local/share/opencode/auth.json << '\''AUTH'\''
'"$auth_json"'
AUTH
    chmod 600 ~/.local/share/opencode/auth.json
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
  local extra_mounts=()
  local memory=""
  local max_memory=""
  local args=()
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --no-git) use_github=false; shift ;;
      --usb) usb_devices+=("$2"); shift 2 ;;
      --usb=*) usb_devices+=("${1#*=}"); shift ;;
      --mount) extra_mounts+=("$2"); shift 2 ;;
      --mount=*) extra_mounts+=("${1#*=}"); shift ;;
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

  # Anthropic auth: use host ~/.claude/.credentials.json if available; the
  # credential proxy MITMs both the Authorization header and OAuth refresh so
  # real tokens never reach the VM.
  local _host_claude_credentials_file
  _host_claude_credentials_file="$(_claude_vm_detect_host_claude_credentials)"
  local _openai_token="${OPENAI_API_KEY:-}"
  local _copilot_token=""
  local _codex_proxy_token="${_openai_token:-}"
  local _codex_host_auth_json=""
  local _codex_placeholder_auth_json=""
  local _opencode_host_auth_json=""
  local _opencode_openai_auth_json=""
  if [ "$agent" = "codex" ] && [ -z "$_openai_token" ]; then
    if _codex_vm_prepare_host_auth; then
      echo "Using host Codex ChatGPT auth via credential proxy."
    else
      _codex_proxy_token=""
      _codex_placeholder_auth_json=""
      echo "Host Codex auth not available or invalid; falling back to VM-local Codex login."
    fi
  elif [ "$agent" = "opencode" ] && [ -z "$_openai_token" ]; then
    if _opencode_vm_prepare_host_auth; then
      echo "Using host Codex ChatGPT auth via native OpenCode OAuth."
    else
      _opencode_openai_auth_json=""
      echo "Host Codex auth not available or invalid; OpenCode will use its configured non-ChatGPT providers."
    fi
  fi

  # Get GitHub token
  _claude_vm_github_repos_json='{}'
  _claude_vm_github_token=""
  if $use_github; then
    _claude_vm_get_github_token "$host_dir" "${extra_mounts[@]}" || echo "Continuing without GitHub..."
    _claude_vm_get_copilot_token
  fi

  # Build credential rules and start unified proxy (if any credentials configured)
  local _credential_rules
  _credential_rules=$(_claude_vm_build_credential_rules "$_codex_proxy_token" "$_claude_vm_github_repos_json" "$_copilot_token" "$_host_claude_credentials_file")
  local _credential_proxy_port=""
  if [ "$_credential_rules" != "[]" ]; then
    _claude_vm_start_credential_proxy "$_credential_rules" || { _claude_vm_cleanup; trap - EXIT INT TERM; return 1; }
  fi

  # Security: snapshot before session
  security_snapshot="$(mktemp)"
  _claude_vm_security_snapshot "$host_dir" "$security_snapshot"

  # Ensure .git/hooks exists for read-only mount
  mkdir -p "${host_dir}/.git/hooks"

  # Build mounts JSON array
  local _mounts_json="{\"location\":\"${host_dir}\",\"writable\":true},{\"location\":\"${host_dir}/.git/hooks\",\"writable\":false},{\"location\":\"${state_dir}\",\"writable\":true}"
  local _m
  for _m in "${extra_mounts[@]}"; do
    _mounts_json="${_mounts_json},{\"location\":\"$(realpath "$_m")\",\"writable\":true}"
  done
  local clone_memory="$memory"
  local _clone_memory_args=()
  if [ -n "$clone_memory" ]; then
    _clone_memory_args=(--memory "$clone_memory")
  fi

  echo "Starting VM '$vm_name'..."
  # Mount project dir writable, but .git/hooks read-only at the Lima level (VM root cannot override)
  # Note: .git/config is a file (not a directory) so it cannot be a Lima mount;
  # it is protected via the pre/post-session security check instead.
  limactl clone "$CLAUDE_VM_TEMPLATE" "$vm_name" \
    --set ".mounts=[${_mounts_json}]" \
    --set '.containerd.system=false' \
    --set '.containerd.user=false' \
    "${_clone_memory_args[@]}" \
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
      _balloon_script="$SCRIPT_DIR/balloon-daemon.py"
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

  _claude_vm_post_boot_setup
  _claude_vm_print_config

  # Update the agent tool to latest version before launching
  if [ "$agent" = "opencode" ]; then
    echo "Updating OpenCode..."
    limactl shell "$vm_name" bash -lc 'opencode update 2>/dev/null || true'
  elif [ "$agent" = "codex" ]; then
    echo "Updating Codex..."
    limactl shell "$vm_name" bash -lc '(curl -fsSL https://github.com/openai/codex/releases/latest/download/install.sh | sh) >/dev/null 2>&1 || true'
  elif [ "$agent" = "copilot" ]; then
    echo "Updating Copilot CLI..."
    limactl shell "$vm_name" bash -lc '(curl -fsSL https://gh.io/copilot-install | bash) >/dev/null 2>&1 || true'
  else
    echo "Updating Claude Code..."
    limactl shell "$vm_name" bash -lc 'claude update --yes 2>/dev/null || true'
  fi

  if [ "$agent" = "opencode" ]; then
    CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
      limactl shell --workdir "$host_dir" "$vm_name" \
      env OPENCODE_CONFIG="${state_dir}/opencode-config/opencode.json" \
      opencode "${args[@]}"
  elif [ "$agent" = "codex" ]; then
    local codex_env=()
    if [ -n "$_openai_token" ]; then
      codex_env+=(OPENAI_API_KEY=dummy-key-auth-handled-by-proxy)
    fi
    CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
      limactl shell --workdir "$host_dir" "$vm_name" \
      env "${codex_env[@]}" \
      codex --dangerously-bypass-approvals-and-sandbox "${args[@]}"
  elif [ "$agent" = "copilot" ]; then
    CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
      limactl shell --workdir "$host_dir" "$vm_name" \
      env copilot --yolo "${args[@]}"
  else
    local claude_args=("--model" "opus[1m]" "${args[@]}")
    CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
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
    ""|claude|opencode|codex|copilot) ;;
    *)
      echo "Error: Unknown agent '$agent' for shell. Use: claude, opencode, codex, or copilot." >&2
      return 1
      ;;
  esac

  local usb_devices=()
  local extra_mounts=()
  local memory=""
  local max_memory=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --no-git) use_github=false; shift ;;
      --usb) usb_devices+=("$2"); shift 2 ;;
      --usb=*) usb_devices+=("${1#*=}"); shift ;;
      --mount) extra_mounts+=("$2"); shift 2 ;;
      --mount=*) extra_mounts+=("${1#*=}"); shift ;;
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

  # Anthropic auth: use host ~/.claude/.credentials.json if available; the
  # credential proxy MITMs both the Authorization header and OAuth refresh so
  # real tokens never reach the VM.
  local _host_claude_credentials_file
  _host_claude_credentials_file="$(_claude_vm_detect_host_claude_credentials)"
  local _openai_token="${OPENAI_API_KEY:-}"
  local _copilot_token=""
  local _codex_proxy_token="${_openai_token:-}"
  local _codex_host_auth_json=""
  local _codex_placeholder_auth_json=""
  local _opencode_host_auth_json=""
  local _opencode_openai_auth_json=""
  if [ -z "$_openai_token" ]; then
    if [ "$agent" = "opencode" ]; then
      if _opencode_vm_prepare_host_auth; then
        echo "Using host Codex ChatGPT auth via native OpenCode OAuth."
      else
        _opencode_openai_auth_json=""
        echo "Host Codex auth not available or invalid; OpenCode will use its configured non-ChatGPT providers."
      fi
    elif [ -z "$agent" ]; then
      if _codex_vm_prepare_host_auth; then
        echo "Using host Codex ChatGPT auth via credential proxy."
      else
        _codex_proxy_token=""
        _codex_placeholder_auth_json=""
        echo "Host Codex auth not available or invalid; falling back to VM-local Codex login."
      fi
      if _opencode_vm_prepare_host_auth; then
        echo "Using host Codex ChatGPT auth via native OpenCode OAuth."
      else
        _opencode_openai_auth_json=""
        echo "Host Codex auth not available or invalid; OpenCode will use its configured non-ChatGPT providers."
      fi
    else
      if _codex_vm_prepare_host_auth; then
        echo "Using host Codex ChatGPT auth via credential proxy."
      else
        _codex_proxy_token=""
        _codex_placeholder_auth_json=""
        echo "Host Codex auth not available or invalid; falling back to VM-local Codex login."
      fi
    fi
  fi

  # Get GitHub token
  _claude_vm_github_repos_json='{}'
  _claude_vm_github_token=""
  if $use_github; then
    _claude_vm_get_github_token "$host_dir" "${extra_mounts[@]}" || echo "Continuing without GitHub..."
    _claude_vm_get_copilot_token
  fi

  # Build credential rules and start unified proxy (if any credentials configured)
  local _credential_rules
  _credential_rules=$(_claude_vm_build_credential_rules "$_codex_proxy_token" "$_claude_vm_github_repos_json" "$_copilot_token" "$_host_claude_credentials_file")
  local _credential_proxy_port=""
  if [ "$_credential_rules" != "[]" ]; then
    _claude_vm_start_credential_proxy "$_credential_rules" || { _claude_vm_shell_cleanup; trap - EXIT INT TERM; return 1; }
  fi

  # Security: snapshot before session
  security_snapshot="$(mktemp)"
  _claude_vm_security_snapshot "$host_dir" "$security_snapshot"

  # Ensure .git/hooks exists for read-only mount
  mkdir -p "${host_dir}/.git/hooks"

  # Build mounts JSON array
  local _mounts_json="{\"location\":\"${host_dir}\",\"writable\":true},{\"location\":\"${host_dir}/.git/hooks\",\"writable\":false},{\"location\":\"${state_dir}\",\"writable\":true}"
  local _m
  for _m in "${extra_mounts[@]}"; do
    _mounts_json="${_mounts_json},{\"location\":\"$(realpath "$_m")\",\"writable\":true}"
  done
  local clone_memory="$memory"
  local _clone_memory_args=()
  if [ -n "$clone_memory" ]; then
    _clone_memory_args=(--memory "$clone_memory")
  fi

  # Mount project dir writable, but .git/hooks read-only at the Lima level (VM root cannot override)
  limactl clone "$CLAUDE_VM_TEMPLATE" "$vm_name" \
    --set ".mounts=[${_mounts_json}]" \
    --set '.containerd.system=false' \
    --set '.containerd.user=false' \
    "${_clone_memory_args[@]}" \
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
      _balloon_script="$SCRIPT_DIR/balloon-daemon.py"
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

  _claude_vm_post_boot_setup

  # Update all agents so they're ready to launch manually
  echo "Updating Claude Code..."
  limactl shell "$vm_name" bash -lc 'claude update --yes 2>/dev/null || true'
  echo "Updating OpenCode..."
  limactl shell "$vm_name" bash -lc 'opencode update 2>/dev/null || true'
  echo "Updating Codex..."
  limactl shell "$vm_name" bash -lc '(curl -fsSL https://github.com/openai/codex/releases/latest/download/install.sh | sh) >/dev/null 2>&1 || true'
  echo "Updating Copilot CLI..."
  limactl shell "$vm_name" bash -lc '(curl -fsSL https://gh.io/copilot-install | bash) >/dev/null 2>&1 || true'

  echo "VM: $vm_name | Dir: $host_dir${agent:+ | Agent: $agent}"
  _claude_vm_print_config
  echo "OpenCode config: ${state_dir}/opencode-config/opencode.json"
  echo "Type 'exit' to stop and delete the VM"
  local shell_env=(
    ENABLE_LSP_TOOL=1
    IS_SANDBOX=1
    OPENCODE_CONFIG="${state_dir}/opencode-config/opencode.json"
  )
  if [ -n "$_openai_token" ]; then
    shell_env+=(OPENAI_API_KEY=dummy-key-auth-handled-by-proxy)
  fi
  CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
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

  if [ $# -eq 0 ]; then
    # List running VMs with current balloon size
    local vm found=false
    while IFS= read -r vm; do
      [ -z "$vm" ] && continue
      [ "$vm" = "$CLAUDE_VM_TEMPLATE" ] && continue
      local sock="$HOME/.lima/$vm/qmp.sock"
      if [ -S "$sock" ]; then
        local size
        size=$(python3 "$SCRIPT_DIR/balloon-daemon.py" "$sock" get 2>/dev/null) || size="?"
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
    python3 "$SCRIPT_DIR/balloon-daemon.py" "$qmp_sock" get
  else
    python3 "$SCRIPT_DIR/balloon-daemon.py" "$qmp_sock" set "$target"
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
    echo "  codex [args]       Run Codex in a sandboxed VM"
    echo "  copilot [args]     Run GitHub Copilot CLI in a sandboxed VM"
    echo "  shell [agent]      Open a debug shell (optionally pre-configured for an agent)"
    echo "  memory [vm] [size] Query or adjust VM memory via balloon (e.g. 'memory 12G')"
    echo ""
    echo "Options (for claude, opencode, codex, copilot, shell):"
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
    codex)
      _agent_vm_run codex "$@"
      ;;
    copilot)
      _agent_vm_run copilot "$@"
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
