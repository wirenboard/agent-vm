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
  # Create a QEMU wrapper with virtio-balloon and optional USB passthrough.
  # Usage: _agent_vm_qemu_wrapper [usb_vid:pid ...]
  local wrapper
  wrapper="$(mktemp /tmp/qemu-wrapper.XXXXXX)"
  local qemu_bin
  qemu_bin="$(command -v qemu-system-x86_64)"
  local extra_args="-device virtio-balloon-pci,id=balloon0,deflate-on-oom=on"
  if [ $# -gt 0 ]; then
    extra_args+=" -device qemu-xhci,id=xhci,addr=0x15"
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

_claude_vm_start_proxy() {
  local host_dir="$1"
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

  echo "Starting API proxy..."
  exec 3< <(CLAUDE_VM_PROXY_DEBUG="${CLAUDE_VM_PROXY_DEBUG:-0}" CLAUDE_VM_PROXY_LOG_DIR="$host_dir" python3 "$script_dir/claude-vm-proxy.py")
  _claude_vm_proxy_pid=$!
  if ! read -r -t 5 _claude_vm_proxy_port <&3; then
    echo "Error: API proxy failed to start." >&2
    kill "$_claude_vm_proxy_pid" 2>/dev/null
    exec 3<&-
    return 1
  fi
  exec 3<&-
  echo "API proxy listening on port $_claude_vm_proxy_port"
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

_claude_vm_write_dummy_credentials() {
  local vm_name="$1"
  # Write dummy credentials so Claude Code detects a Max subscription
  # (selects Opus model, etc.) — real auth is handled by the host proxy
  limactl shell "$vm_name" bash -c 'mkdir -p ~/.claude && cat > ~/.claude/.credentials.json << '\''CREDS'\''
{"claudeAiOauth":{"accessToken":"dummy","refreshToken":"dummy","expiresAt":9999999999999,"scopes":["user:inference","user:profile"],"subscriptionType":"max","rateLimitTier":"default_claude_max_20x"}}
CREDS'
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

_claude_vm_start_github_mcp() {
  local host_dir="$1"
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

  local repos_json='{}'
  local owner="" repo=""

  # Detect main repo from git remote
  local repo_url
  repo_url=$(git -C "$host_dir" remote get-url origin 2>/dev/null)
  if [ -n "$repo_url" ]; then
    read -r owner repo < <(_claude_vm_parse_github_remote "$repo_url") || true
    if [ -n "$owner" ] && [ -n "$repo" ]; then
      echo "Requesting GitHub token for $owner/$repo..."
      local token
      token=$(python3 "$script_dir/github_app_token_demo.py" \
        user-token --client-id Iv23liisR1WdpJmDUPLT \
        --repo "$repo_url" --token-only \
        --cache-dir "$HOME/.cache/claude-vm") || true
      if [ -n "$token" ]; then
        repos_json=$(python3 -c "import json; print(json.dumps({__import__('sys').argv[1]: __import__('sys').argv[2]}))" \
          "$owner/$repo" "$token")
      else
        echo "  Warning: no token for $owner/$repo"
      fi
    fi
  fi

  # Detect GitHub submodules from .gitmodules
  if [ -f "$host_dir/.gitmodules" ]; then
    local sub_urls
    sub_urls=$(python3 -c "
import configparser, sys
p = configparser.ConfigParser()
p.read(sys.argv[1])
for s in p.sections():
    url = p.get(s, 'url', fallback='')
    if 'github.com' in url:
        print(url)
" "$host_dir/.gitmodules")
    local sub_url sub_owner sub_repo sub_token
    while IFS= read -r sub_url; do
      [ -z "$sub_url" ] && continue
      read -r sub_owner sub_repo < <(_claude_vm_parse_github_remote "$sub_url") || continue
      [ -z "$sub_owner" ] || [ -z "$sub_repo" ] && continue
      # Skip if same as main repo
      [ -n "$owner" ] && [ "$sub_owner/$sub_repo" = "$owner/$repo" ] && continue

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
      else
        echo "  Warning: no token for $sub_owner/$sub_repo (skipping credential injection)"
      fi
    done <<< "$sub_urls"
  fi

  # If no repos were configured, skip proxies
  if [ "$repos_json" = '{}' ]; then
    echo "Warning: No GitHub repos found (no remote, no submodules), skipping GitHub MCP" >&2
    return 1
  fi

  # Start GitHub MCP proxy (injects per-repo tokens, enforces repo scope)
  echo "Starting GitHub MCP proxy..."
  exec 4< <(GITHUB_MCP_PROXY_REPOS="$repos_json" \
    GITHUB_MCP_PROXY_DEBUG="${GITHUB_MCP_PROXY_DEBUG:-0}" \
    python3 "$script_dir/github-mcp-proxy.py")
  _claude_vm_github_mcp_pid=$!
  if ! read -r -t 5 _claude_vm_github_mcp_port <&4; then
    echo "Warning: GitHub MCP proxy failed to start" >&2
    kill "$_claude_vm_github_mcp_pid" 2>/dev/null
    exec 4<&-
    return 1
  fi
  exec 4<&-
  local _scope_list
  _scope_list=$(python3 -c "import json,sys; print(', '.join(json.loads(sys.argv[1]).keys()))" "$repos_json")
  echo "GitHub MCP proxy on port $_claude_vm_github_mcp_port (scope: $_scope_list)"

  # Start Git HTTP proxy (injects per-repo tokens for main + submodules)
  echo "Starting Git HTTP proxy..."
  exec 5< <(GITHUB_GIT_PROXY_REPOS="$repos_json" \
    python3 "$script_dir/github-git-proxy.py")
  _claude_vm_git_proxy_pid=$!
  if ! read -r -t 5 _claude_vm_git_proxy_port <&5; then
    echo "Warning: Git HTTP proxy failed to start" >&2
    kill "$_claude_vm_git_proxy_pid" 2>/dev/null
    exec 5<&-
    # Non-fatal: MCP still works, just no git push
  else
    echo "Git HTTP proxy on port $_claude_vm_git_proxy_port"
  fi
  exec 5<&-

  # Export for use by other functions
  _claude_vm_github_repos_json="$repos_json"
}

_claude_vm_inject_github_mcp() {
  local vm_name="$1"
  local port="$2"
  limactl shell "$vm_name" bash -c "
    CONFIG=\$HOME/.claude.json
    if [ -f \"\$CONFIG\" ]; then
      jq '.mcpServers.github = {\"type\":\"http\",\"url\":\"http://host.lima.internal:${port}/mcp\"}' \
        \"\$CONFIG\" > \"\$CONFIG.tmp\" && mv \"\$CONFIG.tmp\" \"\$CONFIG\"
    else
      echo '{\"mcpServers\":{\"github\":{\"type\":\"http\",\"url\":\"http://host.lima.internal:${port}/mcp\"}}}' > \"\$CONFIG\"
    fi
  "
}

_claude_vm_inject_git_proxy() {
  local vm_name="$1"
  local git_port="$2"
  local repos_json="$3"  # JSON dict: {"owner/repo": "token", ...}

  # Get git user identity from host
  local git_name git_email
  git_name=$(git config user.name 2>/dev/null || echo "")
  git_email=$(git config user.email 2>/dev/null || echo "")

  # Set up git insteadOf for each repo in the JSON dict
  # git insteadOf is prefix-matched, so "git@github.com:owner/repo" matches
  # both "git@github.com:owner/repo.git" and "git@github.com:owner/repo".
  local git_config_cmds
  git_config_cmds=$(python3 -c "
import json, sys
repos = json.loads(sys.argv[1])
port = sys.argv[2]
for slug in repos:
    owner, repo = slug.split('/', 1)
    proxy_base = f'http://host.lima.internal:{port}/{owner}/{repo}'
    print(f'git config --global url.{proxy_base}.insteadOf git@github.com:{owner}/{repo}')
    print(f'git config --global --add url.{proxy_base}.insteadOf https://github.com/{owner}/{repo}')
" "$repos_json" "$git_port")

  limactl shell "$vm_name" bash -c "
    $git_config_cmds
    ${git_name:+git config --global user.name \"$git_name\"}
    ${git_email:+git config --global user.email \"$git_email\"}
  "

  # Build list of repos for the CLAUDE.md instructions
  local repo_list
  repo_list=$(python3 -c "
import json, sys
repos = json.loads(sys.argv[1])
slugs = list(repos.keys())
if len(slugs) == 1:
    print(f'Only {slugs[0]} has credentials configured. Other repos will require')
    print('their own authentication.')
else:
    print('The following repositories have credentials configured:')
    for s in slugs:
        print(f'  - {s}')
    print('Other repos will require their own authentication.')
" "$repos_json")

  # Write Claude instructions about git and gh access
  limactl shell "$vm_name" bash -c "
    mkdir -p \$HOME/.claude
    cat >> \$HOME/.claude/CLAUDE.md << INSTRUCTIONS

# Git access

Git push and pull to GitHub work out of the box for this repository.
The remote URL is automatically rewritten to go through a host-side proxy
that injects credentials. You can use standard git commands:

    git push origin main
    git push origin HEAD:my-branch
    git pull origin main

The \`gh\` CLI also works and is pre-authenticated. You can use it for
pull requests, issues, and other GitHub operations:

    gh pr list
    gh pr create --title \"...\" --body \"...\"
    gh issue list

${repo_list}
INSTRUCTIONS
  "
}

_claude_vm_inject_gh_cli() {
  local vm_name="$1"
  local git_port="$2"

  # Create a wrapper script that sets GH_HOST and HTTP_PROXY for gh,
  # routing all gh API traffic through the git proxy
  limactl shell "$vm_name" bash -c '
    mkdir -p "$HOME/.local/bin"
    cat > "$HOME/.local/bin/gh" << '\''WRAPPER'\''
#!/bin/sh
export GH_HOST=github.localhost
export HTTP_PROXY="http://host.lima.internal:'"$git_port"'"
exec /usr/bin/gh "$@"
WRAPPER
    chmod +x "$HOME/.local/bin/gh"
  '

  # Write a minimal gh hosts.yml with a dummy token so gh considers itself
  # authenticated (the real token is injected by the proxy)
  limactl shell "$vm_name" bash -c '
    mkdir -p "$HOME/.config/gh"
    cat > "$HOME/.config/gh/hosts.yml" << '\''HOSTS'\''
github.localhost:
    oauth_token: dummy-token-injected-by-proxy
    user: x-access-token
    git_protocol: https
HOSTS
  '
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
  local proxy_port="$2"
  local state_dir="$3"

  local config_dir="${state_dir}/opencode-config"
  mkdir -p "$config_dir"

  cat > "${config_dir}/opencode.json" << OCJSON
{
  "\$schema": "https://opencode.ai/config.json",
  "provider": {
    "anthropic": {
      "options": {
        "baseURL": "http://host.lima.internal:${proxy_port}/v1",
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

_opencode_vm_inject_github_mcp_opencode() {
  local vm_name="$1"  # unused but kept for consistent API
  local port="$2"
  local state_dir="$3"

  local config="${state_dir}/opencode-config/opencode.json"
  if [ -f "$config" ] && command -v jq &>/dev/null; then
    jq '.mcp.github = {"type":"remote","url":"http://host.lima.internal:'"${port}"'/mcp","enabled":true}' \
      "$config" > "$config.tmp" && mv "$config.tmp" "$config"
  fi
}

_opencode_vm_inject_git_proxy() {
  local vm_name="$1"
  local git_port="$2"
  local repos_json="$3"  # JSON dict: {"owner/repo": "token", ...}

  # Get git user identity from host
  local git_name git_email
  git_name=$(git config user.name 2>/dev/null || echo "")
  git_email=$(git config user.email 2>/dev/null || echo "")

  # Set up git insteadOf for each repo in the JSON dict
  # (Claude's _claude_vm_inject_git_proxy already does this, but we call it
  #  for the opencode-only path where claude's version might not be called)
  local git_config_cmds
  git_config_cmds=$(python3 -c "
import json, sys
repos = json.loads(sys.argv[1])
port = sys.argv[2]
for slug in repos:
    owner, repo = slug.split('/', 1)
    proxy_base = f'http://host.lima.internal:{port}/{owner}/{repo}'
    print(f'git config --global url.{proxy_base}.insteadOf git@github.com:{owner}/{repo}')
    print(f'git config --global --add url.{proxy_base}.insteadOf https://github.com/{owner}/{repo}')
" "$repos_json" "$git_port")

  limactl shell "$vm_name" bash -c "
    $git_config_cmds
    ${git_name:+git config --global user.name \"$git_name\"}
    ${git_email:+git config --global user.email \"$git_email\"}
  "

  # OpenCode reads CLAUDE.md by default (via OPENCODE_DISABLE_CLAUDE_CODE=false),
  # so the git instructions written by _claude_vm_inject_git_proxy are already
  # available to OpenCode. No separate AGENTS.md needed.
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

  _claude_vm_start_proxy "$host_dir" || return 1

  if $use_github; then
    _claude_vm_start_github_mcp "$host_dir" || echo "Continuing without GitHub MCP..."
  fi

  # Security: snapshot before session
  local security_snapshot
  security_snapshot="$(mktemp)"

  _claude_vm_cleanup() {
    echo "Cleaning up VM..."
    [ -n "$_balloon_daemon_pid" ] && kill "$_balloon_daemon_pid" 2>/dev/null
    [ -n "$_claude_vm_proxy_pid" ] && kill "$_claude_vm_proxy_pid" 2>/dev/null
    [ -n "$_claude_vm_github_mcp_pid" ] && kill "$_claude_vm_github_mcp_pid" 2>/dev/null
    [ -n "$_claude_vm_git_proxy_pid" ] && kill "$_claude_vm_git_proxy_pid" 2>/dev/null
    limactl stop "$vm_name" &>/dev/null
    limactl delete "$vm_name" --force &>/dev/null
    # Fall back to rm if limactl delete left a broken directory (missing lima.yaml
    # breaks all future limactl commands)
    [ -d "$HOME/.lima/$vm_name" ] && rm -rf "$HOME/.lima/$vm_name"
    rm -f "$security_snapshot" "${security_snapshot}.git-config"
    local port
    for port in "${_usb_sysfs_ports[@]}"; do
      _agent_vm_usb_rebind "$port"
    done
    [ -n "$_usb_qemu_wrapper" ] && rm -f "$_usb_qemu_wrapper"
  }
  trap _claude_vm_cleanup EXIT INT TERM

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
    QEMU_SYSTEM_X86_64="$_usb_qemu_wrapper" limactl start "$vm_name" &>/dev/null

    # Balloon: set initial target (default 2G) and start auto-balloon daemon
    local _balloon_script
    _balloon_script="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/balloon-daemon.py"
    local _qmp_sock="$HOME/.lima/$vm_name/qmp.sock"
    local _balloon_target="${memory:-2}"
    local _balloon_daemon_args=(--initial-target "${_balloon_target}G")
    if [ -n "$max_memory" ]; then
      _balloon_daemon_args+=(--max-memory "${max_memory}G")
    fi
    python3 "$_balloon_script" "$_qmp_sock" daemon "${_balloon_daemon_args[@]}" &>/dev/null &
    _balloon_daemon_pid=$!
  else
    if [ -n "$max_memory" ]; then
      echo "Warning: --max-memory ignored without balloon (QEMU not available)" >&2
    fi
    if [ -n "$memory" ]; then
      limactl edit "$vm_name" --memory="$memory" --tty=false &>/dev/null
    fi
    limactl start "$vm_name" &>/dev/null
  fi

  _claude_vm_write_dummy_credentials "$vm_name"
  _claude_vm_inject_clipboard_shim "$vm_name" "$state_dir"
  _claude_vm_setup_session_persistence "$vm_name" "$state_dir"
  _claude_vm_ensure_onboarding_config "$vm_name" "$host_dir"

  if [ "$agent" = "opencode" ]; then
    _opencode_vm_setup_config "$vm_name" "$_claude_vm_proxy_port" "$state_dir"
    _opencode_vm_setup_session_persistence "$vm_name" "$state_dir"
    _opencode_vm_setup_auth "$vm_name"
  fi

  if $use_github && [ -n "$_claude_vm_github_mcp_port" ]; then
    _claude_vm_inject_github_mcp "$vm_name" "$_claude_vm_github_mcp_port"
    if [ "$agent" = "opencode" ]; then
      _opencode_vm_inject_github_mcp_opencode "$vm_name" "$_claude_vm_github_mcp_port" "$state_dir"
    fi
  fi
  if $use_github && [ -n "$_claude_vm_git_proxy_port" ] && [ -n "$_claude_vm_github_repos_json" ]; then
    _claude_vm_inject_git_proxy "$vm_name" "$_claude_vm_git_proxy_port" \
      "$_claude_vm_github_repos_json"
    _claude_vm_inject_gh_cli "$vm_name" "$_claude_vm_git_proxy_port"
    if [ "$agent" = "opencode" ]; then
      _opencode_vm_inject_git_proxy "$vm_name" "$_claude_vm_git_proxy_port" \
        "$_claude_vm_github_repos_json"
    fi
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
      env ANTHROPIC_BASE_URL="http://host.lima.internal:${_claude_vm_proxy_port}" \
      IS_SANDBOX=1 \
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

  _claude_vm_start_proxy "$host_dir" || return 1

  if $use_github; then
    _claude_vm_start_github_mcp "$host_dir" || echo "Continuing without GitHub MCP..."
  fi

  # Security: snapshot before session
  local security_snapshot
  security_snapshot="$(mktemp)"

  _claude_vm_shell_cleanup() {
    echo "Cleaning up VM..."
    [ -n "$_balloon_daemon_pid" ] && kill "$_balloon_daemon_pid" 2>/dev/null
    [ -n "$_claude_vm_proxy_pid" ] && kill "$_claude_vm_proxy_pid" 2>/dev/null
    [ -n "$_claude_vm_github_mcp_pid" ] && kill "$_claude_vm_github_mcp_pid" 2>/dev/null
    [ -n "$_claude_vm_git_proxy_pid" ] && kill "$_claude_vm_git_proxy_pid" 2>/dev/null
    limactl stop "$vm_name" &>/dev/null
    limactl delete "$vm_name" --force &>/dev/null
    # Fall back to rm if limactl delete left a broken directory (missing lima.yaml
    # breaks all future limactl commands)
    [ -d "$HOME/.lima/$vm_name" ] && rm -rf "$HOME/.lima/$vm_name"
    rm -f "$security_snapshot" "${security_snapshot}.git-config"
    local port
    for port in "${_usb_sysfs_ports[@]}"; do
      _agent_vm_usb_rebind "$port"
    done
    [ -n "$_usb_qemu_wrapper" ] && rm -f "$_usb_qemu_wrapper"
  }
  trap _claude_vm_shell_cleanup EXIT INT TERM

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
    QEMU_SYSTEM_X86_64="$_usb_qemu_wrapper" limactl start "$vm_name" &>/dev/null

    # Balloon: set initial target (default 2G) and start auto-balloon daemon
    local _balloon_script
    _balloon_script="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/balloon-daemon.py"
    local _qmp_sock="$HOME/.lima/$vm_name/qmp.sock"
    local _balloon_target="${memory:-2}"
    local _balloon_daemon_args=(--initial-target "${_balloon_target}G")
    if [ -n "$max_memory" ]; then
      _balloon_daemon_args+=(--max-memory "${max_memory}G")
    fi
    python3 "$_balloon_script" "$_qmp_sock" daemon "${_balloon_daemon_args[@]}" &>/dev/null &
    _balloon_daemon_pid=$!
  else
    if [ -n "$max_memory" ]; then
      echo "Warning: --max-memory ignored without balloon (QEMU not available)" >&2
    fi
    if [ -n "$memory" ]; then
      limactl edit "$vm_name" --memory="$memory" --tty=false &>/dev/null
    fi
    limactl start "$vm_name" &>/dev/null
  fi

  _claude_vm_write_dummy_credentials "$vm_name"
  _claude_vm_inject_clipboard_shim "$vm_name" "$state_dir"
  _claude_vm_setup_session_persistence "$vm_name" "$state_dir"
  _claude_vm_ensure_onboarding_config "$vm_name" "$host_dir"

  if [ "$agent" = "opencode" ]; then
    _opencode_vm_setup_config "$vm_name" "$_claude_vm_proxy_port" "$state_dir"
    _opencode_vm_setup_session_persistence "$vm_name" "$state_dir"
    _opencode_vm_setup_auth "$vm_name"
  fi

  if $use_github && [ -n "$_claude_vm_github_mcp_port" ]; then
    _claude_vm_inject_github_mcp "$vm_name" "$_claude_vm_github_mcp_port"
    if [ "$agent" = "opencode" ]; then
      _opencode_vm_inject_github_mcp_opencode "$vm_name" "$_claude_vm_github_mcp_port" "$state_dir"
    fi
  fi
  if $use_github && [ -n "$_claude_vm_git_proxy_port" ] && [ -n "$_claude_vm_github_repos_json" ]; then
    _claude_vm_inject_git_proxy "$vm_name" "$_claude_vm_git_proxy_port" \
      "$_claude_vm_github_repos_json"
    _claude_vm_inject_gh_cli "$vm_name" "$_claude_vm_git_proxy_port"
    if [ "$agent" = "opencode" ]; then
      _opencode_vm_inject_git_proxy "$vm_name" "$_claude_vm_git_proxy_port" \
        "$_claude_vm_github_repos_json"
    fi
  fi

  # Run project-specific runtime script if it exists
  if [ -f "${host_dir}/.claude-vm.runtime.sh" ]; then
    limactl shell --workdir "$host_dir" "$vm_name" bash -l < "${host_dir}/.claude-vm.runtime.sh"
  fi

  echo "VM: $vm_name | Dir: $host_dir${agent:+ | Agent: $agent}"
  echo "API proxy: http://host.lima.internal:${_claude_vm_proxy_port}"
  if [ -n "$_claude_vm_github_mcp_port" ]; then
    echo "GitHub MCP: http://host.lima.internal:${_claude_vm_github_mcp_port}/mcp"
  fi
  if [ -n "$_claude_vm_git_proxy_port" ]; then
    echo "Git proxy:  http://host.lima.internal:${_claude_vm_git_proxy_port}"
  fi
  if [ "$agent" = "opencode" ]; then
    echo "OpenCode config: ${state_dir}/opencode-config/opencode.json"
  fi
  echo "Type 'exit' to stop and delete the VM"
  local shell_env=(ANTHROPIC_BASE_URL="http://host.lima.internal:${_claude_vm_proxy_port}" ENABLE_LSP_TOOL=1)
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
