#!/usr/bin/env bash
#
# agent-vm-wsl.sh: Run AI coding agents inside sandboxed WSL2 distros (Windows)
# Part of https://github.com/sylvinus/agent-vm
#
# Source this file in your WSL2 shell config (~/.bashrc):
#   source /path/to/agent-vm/agent-vm-wsl.sh
#
# Usage (same interface as agent-vm on Linux):
#   agent-vm setup            - Create the WSL2 distro template (run once)
#   agent-vm claude [args]    - Run Claude Code in a fresh WSL2 distro
#   agent-vm opencode [args]  - Run OpenCode in a fresh WSL2 distro
#   agent-vm codex [args]     - Run Codex in a fresh WSL2 distro
#   agent-vm copilot [args]   - Run GitHub Copilot CLI in a fresh WSL2 distro
#   agent-vm shell [agent]    - Open a debug shell in a fresh WSL2 distro
#
# Requires: WSL2 on Windows 10/11, wsl.exe accessible via interop,
#           Docker (for Debian base image) or a pre-made debian13-base.tar
#
# Not supported (WSL2 limitations): --usb, --memory/--max-memory balloon

# ── Source shared functions from claude-vm.sh ─────────────────────────────────
# Get the directory of this script at source time
_AGENT_VM_WSL_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"

# Source the main script to inherit all shared helper functions.
# We override agent-vm() and Lima-specific functions below.
# shellcheck source=./claude-vm.sh
source "$_AGENT_VM_WSL_SCRIPT_DIR/claude-vm.sh"

# Re-capture SCRIPT_DIR after sourcing (claude-vm.sh overwrites it)
SCRIPT_DIR="$_AGENT_VM_WSL_SCRIPT_DIR"

# ── WSL2 detection ────────────────────────────────────────────────────────────

_agent_vm_wsl_is_wsl2() {
  [ -f /proc/version ] && grep -qi microsoft /proc/version
}

if ! _agent_vm_wsl_is_wsl2; then
  echo "Warning: agent-vm-wsl.sh requires WSL2. /proc/version does not mention Microsoft." >&2
fi

# ── WSL2 constants ────────────────────────────────────────────────────────────

AGENT_VM_WSL_TEMPLATE_DISTRO="agent-vm-template"

# ── WSL2 management helpers ───────────────────────────────────────────────────

# Normalize wsl.exe output: remove BOM, CR characters, null bytes, empty lines
_wsl_normalize() {
  tr -d '\r\000' | grep -v '^$'
}

_wsl_list_distros() {
  wsl.exe --list --quiet 2>/dev/null | _wsl_normalize
}

_wsl_distro_exists() {
  _wsl_list_distros | grep -qFx "$1"
}

# Run a bash login command as user inside a WSL2 distro
# Equivalent of: limactl shell "$vm" bash -lc "..."
_wsl_run() {
  local distro="$1"; shift
  wsl.exe -d "$distro" -u user -- bash -lc "$*"
}

# Run a bash command as root inside a WSL2 distro
_wsl_run_root() {
  local distro="$1"; shift
  wsl.exe -d "$distro" -u root -- bash -c "$*"
}

# Pipe stdin as bash input into a WSL2 distro (equivalent of: limactl shell $vm bash < file)
_wsl_pipe_as_user() {
  local distro="$1"
  wsl.exe -d "$distro" -u user -- bash
}

# Run a bash script in a WSL2 distro via a temp file on the Windows drive.
# Avoids binfmt_misc interop problems:
#   1. Newline-to-space mangling in bash -c arguments
#   2. wsl.exe silently exits when stdin is at EOF
#   3. cmd.exe/wslpath inside _wsl_win_home (command substitution) inherits stdin
#      and consumes heredoc bytes before cat can read them — so we read stdin first.
#   4. wsl.exe -u user tries chdir(/mnt/c/Users/user) before applying --cd, and
#      gets EACCES on fresh distros where DrvFs uid mapping isn't set up yet.
#      Fixed by running as root and using `su - user` inside the distro instead.
# Usage: _wsl_run_script <distro> <user> << 'EOF' ... EOF
_wsl_run_script() {
  local distro="$1"
  local run_user="${2:-root}"
  # Read stdin FIRST: $(cmd.exe) inside _wsl_win_home inherits stdin via binfmt_misc
  # interop and consumes heredoc bytes, leaving cat with an empty stream.
  local script
  script=$(cat)
  local win_tmp; win_tmp="$(_wsl_win_home)/AppData/Local/Temp/agent-vm-$$-${RANDOM}.sh"
  printf '%s\n' "$script" > "$win_tmp"
  if [ "$run_user" = "root" ]; then
    wsl.exe -d "$distro" -u root -- bash "$win_tmp" < /dev/null
  else
    # Don't use wsl.exe -u user: the relay tries chdir(/mnt/c/Users/user) before
    # applying --cd and gets EACCES on fresh distros. Run as root and switch via
    # sudo -u (not su -): sudo does not open a PAM session so it won't create a
    # lingering systemd user session that blocks wsl.exe --terminate / --export.
    local distro_tmp="/tmp/agent-vm-$$-${RANDOM}.sh"
    wsl.exe -d "$distro" -u root -- bash -c "cp '$win_tmp' '$distro_tmp' && chmod 755 '$distro_tmp' && sudo -u '$run_user' env HOME=/home/'$run_user' bash '$distro_tmp'; rc=\$?; rm -f '$distro_tmp'; exit \$rc" < /dev/null
  fi
  local rc=$?
  rm -f "$win_tmp" 2>/dev/null || true
  return $rc
}

# Get Windows path from a WSL path (handles both /home/... and /mnt/c/... styles).
# Use wslpath -m (mixed: C:/... with forward slashes) rather than -w (C:\...) because
# wsl.exe --export fails with backslash paths for Modern imported distros in WSL 2.6+.
_wsl_win_path() {
  wslpath -m "$1" 2>/dev/null || echo "$1"
}

# Get the Windows user's home directory as a WSL2 /mnt/c/... path.
# Used to resolve a Windows-native base for wsl --import/--export.
_wsl_win_home() {
  local win_home; win_home="$(cmd.exe /C "echo %USERPROFILE%" 2>/dev/null | tr -d '\r\n')"
  if [ -n "$win_home" ]; then
    wslpath -u "$win_home" 2>/dev/null || echo "/mnt/c/Users/${USER}"
  else
    echo "/mnt/c/Users/${USER}"
  fi
}

# Where the template tar and per-run instances live.
# MUST be on a Windows-native drive (C:\...) so that wsl.exe --import/--export
# can accept the paths directly — wsl.exe does not accept \\wsl.localhost UNC paths.
_wsl_data_dir() {
  local win_home; win_home="$(_wsl_win_home)"
  printf '%s\n' "${win_home}/AppData/Local/agent-vm"
}

_wsl_template_tar() {
  printf '%s\n' "$(_wsl_data_dir)/template.tar"
}

_wsl_instances_dir() {
  printf '%s\n' "$(_wsl_data_dir)/instances"
}

# Override _agent_vm_state_root from claude-vm.sh:
# Use Windows AppData so that per-project state dirs live on the C: drive.
# drvfs auto-mounts C:\ in EVERY WSL2 distro, so the agent's 'user' can always
# reach the state dir without needing a separate bind mount.
_agent_vm_state_root() {
  if [ -n "${AGENT_VM_STATE_DIR:-}" ]; then
    printf '%s\n' "$AGENT_VM_STATE_DIR"
  else
    local win_home; win_home="$(_wsl_win_home)"
    printf '%s\n' "${win_home}/.local/state/agent-vm"
  fi
}

# Import template tar as a new named distro (equivalent of limactl clone)
_wsl_import_template() {
  local distro="$1"
  local instance_dir; instance_dir="$(_wsl_instances_dir)/$distro"
  mkdir -p "$instance_dir"
  local win_instance_dir; win_instance_dir="$(_wsl_win_path "$instance_dir")"
  local win_template_tar; win_template_tar="$(_wsl_win_path "$(_wsl_template_tar)")"
  wsl.exe --import "$distro" "$win_instance_dir" "$win_template_tar" --version 2
}

# Shared tmpfs mount point root for cross-distro directory sharing.
# All running WSL2 distros see the same /mnt/wsl/ namespace.
_wsl_shared_mnt_root() {
  echo "/mnt/wsl/agent-vm"
}

# Mount a HOST distro directory into a running AGENT distro.
#
# Mechanism: bind-mount the path under /mnt/wsl/ from the HOST distro;
# the bind mount is visible to the AGENT distro because all WSL2 distros
# share the same /mnt/wsl/ tmpfs.  A symlink inside the AGENT then makes
# the path available at its original location.
#
# For paths already under /mnt/ (Windows drives), they're auto-mounted in
# every distro — just create the directory in the agent and skip binding.
_wsl_mount_dir() {
  local distro="$1"
  local host_path="$2"
  local guest_path="${3:-$host_path}"
  local readonly="${4:-false}"

  # Windows drive paths (/mnt/c/...) are auto-mounted in all distros
  if [[ "$host_path" == /mnt/[a-zA-Z]/* || "$host_path" == /mnt/[a-zA-Z] ]]; then
    wsl.exe -d "$distro" -u root -- bash -c "mkdir -p '$guest_path'" 2>/dev/null || true
    return 0
  fi

  # Use /mnt/wsl/ as the shared namespace for cross-distro bind mounts.
  # The bind mount created in HOST is visible in AGENT via the shared tmpfs.
  local shared_path; shared_path="$(_wsl_shared_mnt_root)/${distro}${host_path}"
  local bind_opts="--bind"
  if [ "$readonly" = "true" ]; then
    bind_opts="--bind -o ro"
  fi

  # HOST side: create the shared mount point and bind-mount the path there
  sudo mkdir -p "$shared_path"
  sudo mount $bind_opts "$host_path" "$shared_path" 2>/dev/null || {
    echo "Warning: bind mount of '$host_path' failed, trying without sudo..." >&2
    mkdir -p "$shared_path"
    mount $bind_opts "$host_path" "$shared_path" 2>/dev/null || true
  }

  # AGENT side: create a symlink (or directory) so the path is at guest_path
  if [ "$guest_path" != "$shared_path" ]; then
    _wsl_run_script "$distro" "root" << EOF
mkdir -p "$(dirname "$guest_path")"
ln -sfn "$shared_path" "$guest_path" 2>/dev/null || true
EOF
  fi
}

# Unmount all shared directories for a distro from /mnt/wsl/
_wsl_unmount_shared() {
  local distro="$1"
  local shared_root; shared_root="$(_wsl_shared_mnt_root)/${distro}"
  if [ -d "$shared_root" ]; then
    # Unmount all bind mounts under this distro's share root
    mount | grep "$shared_root" | awk '{print $3}' | sort -r | while read -r mnt; do
      sudo umount "$mnt" 2>/dev/null || true
    done
    rm -rf "$shared_root" 2>/dev/null || true
  fi
}

# In WSL2, all distros share the same network namespace (same Hyper-V VM),
# so the credential proxy bound on the host is reachable at localhost.
_wsl_credential_proxy_host() {
  echo "localhost"
}

# ── Override _claude_vm_print_config for WSL2 ────────────────────────────────

_claude_vm_print_config() {
  if [ -n "${_credential_proxy_port:-}" ]; then
    echo "Credential proxy: http://localhost:${_credential_proxy_port}"
    echo "Intercepted domains: ${_intercepted_domains:-}"
  fi
  if [ -n "${_oauth_token:-}" ]; then
    local _truncated="${_oauth_token:0:16}"
    [ ${#_oauth_token} -gt 16 ] && _truncated="${_truncated}..."
    echo "VM env: CLAUDE_CODE_OAUTH_TOKEN=${_truncated}"
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
}

# ── Setup: build the WSL2 template distro ─────────────────────────────────────

_agent_vm_setup() {
  local minimal=false

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --help|-h)
        echo "Usage: agent-vm setup [--minimal]"
        echo ""
        echo "Create a WSL2 distro template with Claude Code, OpenCode, Codex, and Copilot CLI."
        echo ""
        echo "Options:"
        echo "  --minimal      Only install git, curl, jq, Claude Code, OpenCode, Codex, Copilot CLI"
        echo "  --help         Show this help"
        return 0
        ;;
      --minimal) minimal=true; shift ;;
      # Silently accept Lima-specific flags for portability
      --disk|--memory) shift 2 ;;
      --disk=*|--memory=*) shift ;;
      *) echo "Unknown option: $1" >&2; return 1 ;;
    esac
  done

  if ! command -v wsl.exe &>/dev/null; then
    echo "Error: wsl.exe not found. This script must run inside a WSL2 distro." >&2
    return 1
  fi

  local data_dir; data_dir="$(_wsl_data_dir)"
  local template_tar; template_tar="$(_wsl_template_tar)"
  mkdir -p "$data_dir"

  # Remove existing template distro if present
  if _wsl_distro_exists "$AGENT_VM_WSL_TEMPLATE_DISTRO"; then
    wsl.exe --terminate "$AGENT_VM_WSL_TEMPLATE_DISTRO" 2>/dev/null || true
    wsl.exe --unregister "$AGENT_VM_WSL_TEMPLATE_DISTRO" 2>/dev/null || true
  fi

  # Acquire Debian 13 (trixie) base rootfs
  local base_tar="${data_dir}/debian13-base.tar"
  if [ ! -f "$base_tar" ]; then
    echo "Creating Debian 13 base image..."
    _wsl_get_debian_base "$base_tar" || return 1
  fi

  echo "Creating WSL2 template distro '$AGENT_VM_WSL_TEMPLATE_DISTRO'..."
  local setup_dir="${data_dir}/distro-setup"
  mkdir -p "$setup_dir"
  local win_setup_dir; win_setup_dir="$(_wsl_win_path "$setup_dir")"
  local win_base_tar; win_base_tar="$(_wsl_win_path "$base_tar")"

  wsl.exe --import "$AGENT_VM_WSL_TEMPLATE_DISTRO" "$win_setup_dir" "$win_base_tar" --version 2

  # WSL2 export captures the ext4 inode mode for /, which is 700 in distros
  # exported by WSL2 (the kernel presents it as 755 to processes inside, but the
  # raw inode is 700). Fix it immediately so non-root users can traverse paths.
  wsl.exe -d "$AGENT_VM_WSL_TEMPLATE_DISTRO" -u root -- bash -c 'chmod 755 /'

  echo "Configuring base system..."
  wsl.exe -d "$AGENT_VM_WSL_TEMPLATE_DISTRO" -u root -- bash -c '
    # Create non-root user account
    groupadd -g 1000 user 2>/dev/null || true
    id user &>/dev/null || useradd -m -s /bin/bash -u 1000 -g 1000 user
    apt-get install -y sudo 2>/dev/null || true
    echo "user ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/user
    chmod 440 /etc/sudoers.d/user

    # WSL2 config: set default user and enable systemd
    cat > /etc/wsl.conf << '"'"'EOF'"'"'
[user]
default=user
[boot]
systemd=true
EOF

    # Ensure hostname resolves
    grep -q "$(hostname)" /etc/hosts 2>/dev/null || echo "127.0.0.1 $(hostname)" >> /etc/hosts
  '

  # Disable needrestart interactive prompts
  wsl.exe -d "$AGENT_VM_WSL_TEMPLATE_DISTRO" -u root -- bash -c '
    mkdir -p /etc/needrestart/conf.d
    printf "\$nrconf{restart} = '"'"'a'"'"';\n" > /etc/needrestart/conf.d/no-prompt.conf
  ' 2>/dev/null || true

  echo "Installing base packages..."
  wsl.exe -d "$AGENT_VM_WSL_TEMPLATE_DISTRO" -u root -- \
    env DEBIAN_FRONTEND=noninteractive bash -c '
      apt-get update -qq
      apt-get install -y git curl jq gh ca-certificates sudo python3 python3-pip
    '

  _claude_vm_install_host_proxy_ca_wsl "$AGENT_VM_WSL_TEMPLATE_DISTRO"

  if ! $minimal; then
    echo "Installing dev tools..."
    wsl.exe -d "$AGENT_VM_WSL_TEMPLATE_DISTRO" -u root -- \
      env DEBIAN_FRONTEND=noninteractive bash -c '
        apt-get install -y \
          wget build-essential \
          python3 python3-pip python3-venv \
          ripgrep fd-find htop \
          unzip zip \
          mosquitto-clients
      '

    echo "Installing Docker..."
    wsl.exe -d "$AGENT_VM_WSL_TEMPLATE_DISTRO" -u root -- bash -c '
      install -m 0755 -d /etc/apt/keyrings
      curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
      chmod a+r /etc/apt/keyrings/docker.asc
      echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] \
        https://download.docker.com/linux/debian \
        $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
        | tee /etc/apt/sources.list.d/docker.list > /dev/null
      apt-get update -qq
      apt-get install -y docker-ce docker-ce-cli containerd.io docker-compose-plugin
      usermod -aG docker user
    '

    echo "Installing Node.js 22..."
    wsl.exe -d "$AGENT_VM_WSL_TEMPLATE_DISTRO" -u root -- bash -c '
      curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
      apt-get install -y nodejs
    '

    echo "Installing Chromium..."
    wsl.exe -d "$AGENT_VM_WSL_TEMPLATE_DISTRO" -u root -- \
      env DEBIAN_FRONTEND=noninteractive bash -c '
        apt-get install -y chromium fonts-liberation xvfb
        ln -sf /usr/bin/chromium /usr/bin/google-chrome
        ln -sf /usr/bin/chromium /usr/bin/google-chrome-stable
        mkdir -p /opt/google/chrome
        ln -sf /usr/bin/chromium /opt/google/chrome/chrome
      '

    echo "Installing LSP servers..."
    wsl.exe -d "$AGENT_VM_WSL_TEMPLATE_DISTRO" -u root -- \
      env DEBIAN_FRONTEND=noninteractive apt-get install -y clangd golang-go
    _wsl_run_script "$AGENT_VM_WSL_TEMPLATE_DISTRO" "user" << 'EOF'
export PATH=$HOME/.local/bin:$PATH
GOBIN=$HOME/.local/bin go install golang.org/x/tools/gopls@latest 2>/dev/null || true
sudo npm install -g typescript-language-server typescript pyright 2>/dev/null || true
EOF
  fi

  echo "Installing mitmproxy..."
  wsl.exe -d "$AGENT_VM_WSL_TEMPLATE_DISTRO" -u root -- bash -c '
    python3 -m pip install --break-system-packages --ignore-installed bcrypt mitmproxy 2>/dev/null || \
    python3 -m pip install --ignore-installed bcrypt mitmproxy
  '

  echo "Installing Claude Code..."
  _wsl_run_script "$AGENT_VM_WSL_TEMPLATE_DISTRO" "user" << 'EOF'
curl -fsSL https://claude.ai/install.sh | bash
EOF

  # Write PATH additions to .profile. Use _wsl_run_script so the heredoc runs in a
  # real script file (not via bash -c through binfmt_misc which mangles quoting).
  # .bashrc has an early-return for non-interactive shells, so PATH set there is
  # invisible to "wsl.exe ... bash -lc '...'" commands; .profile is the right place.
  _wsl_run_script "$AGENT_VM_WSL_TEMPLATE_DISTRO" "root" << 'EOF'
cat >> /home/user/.profile << 'PROFEOF'

# agent-vm: agent tools PATH
export PATH=$HOME/.local/bin:$HOME/.claude/local/bin:$HOME/.opencode/bin:$PATH
PROFEOF
chown user:user /home/user/.profile
EOF

  # Skip first-run onboarding
  _wsl_run_script "$AGENT_VM_WSL_TEMPLATE_DISTRO" "user" << 'EOF'
mkdir -p ~/.claude
printf '{"theme":"dark","hasCompletedOnboarding":true,"skipDangerousModePermissionPrompt":true,"effortLevel":"high"}' \
  > ~/.claude/settings.json
EOF

  # Pre-install LSP plugins
  echo "Installing Claude Code LSP plugins..."
  _wsl_run_script "$AGENT_VM_WSL_TEMPLATE_DISTRO" "user" << 'EOF'
export PATH=$HOME/.local/bin:$HOME/.claude/local/bin:$PATH
MARKETPLACE_DIR="$HOME/.claude/plugins/marketplaces/claude-plugins-official"
mkdir -p "$HOME/.claude/plugins/marketplaces"
git clone --depth 1 https://github.com/anthropics/claude-plugins-official.git "$MARKETPLACE_DIR" 2>/dev/null || true
cat > "$HOME/.claude/plugins/known_marketplaces.json" << MKTJSON
{"claude-plugins-official":{"source":{"source":"github","repo":"anthropics/claude-plugins-official"},"installLocation":"$HOME/.claude/plugins/marketplaces/claude-plugins-official","lastUpdated":"2026-01-01T00:00:00.000Z"}}
MKTJSON
for plugin in clangd-lsp pyright-lsp typescript-lsp gopls-lsp; do
  claude plugin install "${plugin}@claude-plugins-official" --scope user 2>/dev/null || true
done
EOF

  echo "Installing OpenCode..."
  _wsl_run_script "$AGENT_VM_WSL_TEMPLATE_DISTRO" "user" << 'EOF'
curl -fsSL https://opencode.ai/install | bash
mkdir -p ~/.local/bin
ln -sf ~/.opencode/bin/opencode ~/.local/bin/opencode 2>/dev/null || true
EOF

  echo "Installing Codex..."
  _wsl_run_script "$AGENT_VM_WSL_TEMPLATE_DISTRO" "user" << 'EOF'
curl -fsSL https://github.com/openai/codex/releases/latest/download/install.sh | sh
EOF

  echo "Installing GitHub Copilot CLI..."
  _wsl_run_script "$AGENT_VM_WSL_TEMPLATE_DISTRO" "user" << 'EOF'
# Installer opens /dev/tty directly (bypasses < /dev/null). Rewrite to /dev/stdin
# so the PATH-update prompt gets EOF (= N = default; agent-vm owns PATH in .profile).
curl -fsSL https://gh.io/copilot-install | sed 's|/dev/tty|/dev/stdin|g' | bash < /dev/null
EOF

  if ! $minimal; then
    echo "Configuring Chrome DevTools MCP server..."
    _wsl_run_script "$AGENT_VM_WSL_TEMPLATE_DISTRO" "user" << 'EOF'
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
EOF
  fi

  # Run user's custom setup script if present
  local user_setup="$HOME/.claude-vm.setup.sh"
  if [ -f "$user_setup" ]; then
    echo "Running custom setup from $user_setup..."
    _wsl_run_script "$AGENT_VM_WSL_TEMPLATE_DISTRO" "user" < "$user_setup"
  fi

  echo "Exporting template..."
  # Ensure no user processes remain (sudo -u does not create systemd sessions, but
  # be defensive in case something else did — loginctl kill-user cleans it up).
  wsl.exe -d "$AGENT_VM_WSL_TEMPLATE_DISTRO" -u root -- bash -c 'pkill -u user 2>/dev/null; loginctl kill-user user 2>/dev/null; true' < /dev/null 2>/dev/null || true
  wsl.exe --terminate "$AGENT_VM_WSL_TEMPLATE_DISTRO" 2>/dev/null || true

  # --terminate only REQUESTS shutdown; with systemd=true the distro can take
  # 15-90 seconds to fully stop. Poll wsl --list --verbose until it shows
  # "Stopped" before attempting export (which fails with 0x8000000d otherwise).
  local _wait=0
  while wsl.exe --list --verbose 2>/dev/null | tr -d '\r\000' | grep -q "$AGENT_VM_WSL_TEMPLATE_DISTRO.*Running"; do
    [ $_wait -eq 0 ] && echo "  Waiting for distro to stop..."
    sleep 3
    _wait=$((_wait + 3))
    if [ $_wait -ge 120 ]; then
      echo "  Warning: distro still running after 120s, proceeding anyway..."
      break
    fi
  done

  local win_template_tar; win_template_tar="$(_wsl_win_path "$template_tar")"
  echo "  Exporting to: $win_template_tar"
  local _exported=false
  for _i in 1 2 3 4 5 6 7 8 9 10; do
    if wsl.exe --export "$AGENT_VM_WSL_TEMPLATE_DISTRO" "$win_template_tar"; then
      _exported=true
      break
    fi
    echo "  Export attempt $_i failed, retrying in 5s..."
    sleep 5
  done
  if ! $_exported; then
    echo "Error: Failed to export template after 10 attempts." >&2
    return 1
  fi

  # Clean up the setup distro — only the tar is kept.
  # Retry because binfmt_misc can briefly drop after --terminate.
  for _i in 1 2 3 4 5; do
    if wsl.exe --unregister "$AGENT_VM_WSL_TEMPLATE_DISTRO" 2>/dev/null; then
      break
    fi
    sleep 2
  done
  rm -rf "$setup_dir" 2>/dev/null || true

  echo "Template ready. Run 'agent-vm claude', 'agent-vm opencode', or 'agent-vm codex' in any project directory."
}

_wsl_get_debian_base() {
  local output_tar="$1"

  # Method 1: Docker (preferred — available on most dev machines)
  if command -v docker &>/dev/null && docker info &>/dev/null 2>&1; then
    echo "  Building Debian 13 rootfs via Docker..."
    local cid
    cid=$(docker create debian:trixie)
    docker export "$cid" > "$output_tar"
    docker rm "$cid" &>/dev/null
    echo "  Debian 13 base image created."
    return 0
  fi

  # Method 2: export an existing Debian/Ubuntu WSL2 distro as the base.
  # This is the typical case on a Windows machine that has a Debian/Ubuntu Store app installed.
  # Skip the current distro (WSL_DISTRO_NAME) since we can't terminate ourselves.
  local _current_distro="${WSL_DISTRO_NAME:-}"
  local _existing
  _existing=$(wsl.exe --list --quiet 2>/dev/null | tr -d '\r\000' \
    | grep -iE '^(Debian|Ubuntu|Ubuntu-[0-9]+\.[0-9]+)$' \
    | grep -v "^${_current_distro}$" \
    | head -1)
  if [ -n "$_existing" ]; then
    echo "  Exporting '$_existing' as base image..."
    local win_output_tar; win_output_tar="$(_wsl_win_path "$output_tar")"
    wsl.exe --terminate "$_existing" 2>/dev/null || true
    if wsl.exe --export "$_existing" "$win_output_tar"; then
      echo "  Base image created from '$_existing'."
      return 0
    fi
    echo "  Warning: export of '$_existing' failed, falling through to debootstrap..." >&2
  fi

  # Method 3: debootstrap (if available or installable via apt)
  if ! command -v debootstrap &>/dev/null && command -v apt-get &>/dev/null; then
    echo "  Installing debootstrap..."
    sudo apt-get install -y -qq debootstrap 2>/dev/null || true
  fi
  if command -v debootstrap &>/dev/null; then
    echo "  Building Debian 13 rootfs via debootstrap..."
    local tmpdir; tmpdir="$(mktemp -d)"
    sudo debootstrap --arch=amd64 trixie "$tmpdir" https://deb.debian.org/debian
    # Ensure root dir has world-executable permission (mktemp -d creates 0700 which would
    # break non-root users inside the imported WSL2 distro)
    sudo chmod 755 "$tmpdir"
    sudo tar -C "$tmpdir" -cf "$output_tar" .
    sudo rm -rf "$tmpdir"
    echo "  Debian 13 base image created."
    return 0
  fi

  cat >&2 << 'EOF'
Error: Cannot create Debian 13 base image.

No suitable base found. To create it manually, one of:

  Option A — Export your existing Debian/Ubuntu WSL2 distro:
    wsl --export Debian "%USERPROFILE%\.local\share\agent-vm\debian13-base.tar"

  Option B — Docker (on Windows host, Docker Desktop):
    docker export $(docker create debian:trixie) > %USERPROFILE%\.local\share\agent-vm\debian13-base.tar

  Option C — Any Linux machine with debootstrap:
    debootstrap --arch=amd64 trixie /tmp/debian13 https://deb.debian.org/debian
    tar -C /tmp/debian13 -cf ~/.local/share/agent-vm/debian13-base.tar .
EOF
  return 1
}

_claude_vm_install_host_proxy_ca_wsl() {
  local distro="$1"
  local host_ca="$HOME/.mitmproxy/mitmproxy-ca-cert.pem"
  [ -f "$host_ca" ] || return 0

  echo "Installing host MITM CA into WSL2 distro trust store..."
  cat "$host_ca" | wsl.exe -d "$distro" -u root -- tee /usr/local/share/ca-certificates/host-mitmproxy-ca.crt > /dev/null
  wsl.exe -d "$distro" -u root -- update-ca-certificates
  wsl.exe -d "$distro" -u user -- git config --global http.sslCAInfo /etc/ssl/certs/ca-certificates.crt
}

# ── Per-distro post-boot setup (WSL2 equivalent of _claude_vm_post_boot_setup) ─

_claude_vm_post_boot_setup_wsl() {
  # Called with these variables in scope (set by the caller):
  #   distro, host_dir, state_dir, agent, use_github,
  #   _anthropic_token, _openai_token, _codex_proxy_token, _copilot_token,
  #   _codex_placeholder_auth_json, _credential_rules, _credential_proxy_port,
  #   _credential_proxy_secret, _claude_vm_github_repos_json
  # Sets: _oauth_token, _codex_home, _intercepted_domains

  _wsl_inject_clipboard_shim "$distro" "$state_dir"
  _oauth_token=""
  _codex_home=""

  if [ "$agent" = "claude" ] || [ "$agent" = "opencode" ] || [ -z "$agent" ]; then
    _oauth_token="${_anthropic_token:+placeholder}"
    _oauth_token="${_oauth_token:-${AI_HTTPS_PROXY:+placeholder}}"
    _wsl_write_oauth_token "$distro" "$_oauth_token"
    _wsl_setup_session_persistence "$distro" "$state_dir"
    _wsl_ensure_onboarding_config "$distro" "$host_dir"
  fi

  if [ "$agent" = "codex" ] || [ -z "$agent" ]; then
    _wsl_codex_setup_home "$distro" "$state_dir" "${_codex_placeholder_auth_json:-}"
    _wsl_codex_write_api_key "$distro" "${_openai_token:-}"
    _codex_home="${state_dir}/codex-home"
  fi

  if [ "$agent" = "copilot" ] || [ -z "$agent" ]; then
    _wsl_copilot_setup_home "$distro" "$state_dir"
  fi

  _wsl_copilot_write_token "$distro" "${_copilot_token:-}"

  _intercepted_domains=""
  if [ -n "${_credential_rules:-}" ] && [ "$_credential_rules" != "[]" ]; then
    echo "Setting up mitmproxy..."
    _wsl_setup_mitmproxy "$distro"

    _intercepted_domains=$(python3 -c "
import json, sys
rules = json.loads(sys.argv[1])
seen = dict.fromkeys(r['domain'] for r in rules)
print(','.join(seen))
" "$_credential_rules")

    _wsl_start_mitmproxy "$distro" "$_credential_proxy_port" "$_intercepted_domains"
  fi

  if $use_github && [ "$_claude_vm_github_repos_json" != '{}' ]; then
    _wsl_inject_git_credentials "$distro"
    _wsl_inject_gh_credentials "$distro"
    _wsl_write_instructions "$distro" "$_claude_vm_github_repos_json"

    if [ "$agent" = "codex" ] || [ -z "$agent" ]; then
      wsl.exe -d "$distro" -u user -- bash -c \
        'ln -sf "$HOME/.claude/CLAUDE.md" "$HOME/.codex/AGENTS.md"'
    fi

    if [ "$agent" = "copilot" ] || [ -z "$agent" ]; then
      wsl.exe -d "$distro" -u user -- bash -c \
        'mkdir -p "$HOME/.copilot" && ln -sf "$HOME/.claude/CLAUDE.md" "$HOME/.copilot/copilot-instructions.md"'
    fi
  fi

  if [ "$agent" = "opencode" ] || [ -z "$agent" ]; then
    _wsl_opencode_setup_config "$distro" "$state_dir"
    _wsl_opencode_setup_session_persistence "$distro" "$state_dir"
    _wsl_opencode_setup_auth "$distro"
  fi

  # Run project-specific runtime script if present
  if [ -f "${host_dir}/.claude-vm.runtime.sh" ]; then
    echo "Running project runtime setup..."
    wsl.exe -d "$distro" -u user -- bash -l < "${host_dir}/.claude-vm.runtime.sh"
  fi
}

# ── Individual WSL2 setup helpers ─────────────────────────────────────────────

_wsl_inject_clipboard_shim() {
  local distro="$1"
  local state_dir="$2"
  _wsl_run_script "$distro" "user" << EOF
mkdir -p ~/.local/bin

cat > ~/.local/bin/wl-paste << 'SHIM'
#!/bin/sh
CLIPBOARD_FILE="${state_dir}/clipboard.png"
for arg in "\$@"; do
  case "\$arg" in
    -l|--list-types)
      if [ -f "\$CLIPBOARD_FILE" ]; then echo "image/png"; exit 0
      else exit 1; fi
      ;;
  esac
done
if [ -f "\$CLIPBOARD_FILE" ]; then exec cat "\$CLIPBOARD_FILE"; fi
exit 1
SHIM
chmod +x ~/.local/bin/wl-paste

cat > ~/.local/bin/xclip << 'XSHIM'
#!/bin/sh
CLIPBOARD_FILE="${state_dir}/clipboard.png"
has_targets=false; has_output=false
for arg in "\$@"; do
  case "\$arg" in TARGETS) has_targets=true;; -o) has_output=true;; esac
done
if \$has_targets && \$has_output; then
  [ -f "\$CLIPBOARD_FILE" ] && echo "image/png" && exit 0; exit 1
fi
if \$has_output; then
  [ -f "\$CLIPBOARD_FILE" ] && exec cat "\$CLIPBOARD_FILE"; exit 1
fi
exit 1
XSHIM
chmod +x ~/.local/bin/xclip
EOF
}

_wsl_write_oauth_token() {
  local distro="$1"
  local token="$2"
  [ -z "$token" ] && return 0
  _wsl_run_script "$distro" "root" << EOF
tee /etc/profile.d/claude-oauth.sh > /dev/null << 'TOKENEOF'
export CLAUDE_CODE_OAUTH_TOKEN=${token}
TOKENEOF
chmod 644 /etc/profile.d/claude-oauth.sh
EOF
}

_wsl_codex_write_api_key() {
  local distro="$1"
  local token="$2"
  [ -z "$token" ] && return 0
  _wsl_run_script "$distro" "root" << 'EOF'
tee /etc/profile.d/codex-api-key.sh > /dev/null << 'KEYEOF'
export OPENAI_API_KEY=dummy-key-auth-handled-by-proxy
KEYEOF
chmod 644 /etc/profile.d/codex-api-key.sh
EOF
}

_wsl_copilot_write_token() {
  local distro="$1"
  local token="$2"
  [ -z "$token" ] && return 0
  _wsl_run_script "$distro" "root" << 'EOF'
tee /etc/profile.d/copilot-token.sh > /dev/null << 'TOKEOF'
export COPILOT_GITHUB_TOKEN=placeholder-copilot-token-injected-by-proxy
TOKEOF
chmod 644 /etc/profile.d/copilot-token.sh
EOF
}

_wsl_setup_session_persistence() {
  local distro="$1"
  local state_dir="$2"
  _wsl_run_script "$distro" "user" << EOF
SESSION_DIR="${state_dir}/claude-sessions"
mkdir -p ~/.claude \\
  "\$SESSION_DIR/projects" \\
  "\$SESSION_DIR/file-history" \\
  "\$SESSION_DIR/todos" \\
  "\$SESSION_DIR/plans"
ln -sfn "\$SESSION_DIR/projects" ~/.claude/projects
ln -sfn "\$SESSION_DIR/file-history" ~/.claude/file-history
ln -sfn "\$SESSION_DIR/todos" ~/.claude/todos
ln -sfn "\$SESSION_DIR/plans" ~/.claude/plans
touch "\$SESSION_DIR/history.jsonl"
ln -sfn "\$SESSION_DIR/history.jsonl" ~/.claude/history.jsonl
if [ ! -f "\$SESSION_DIR/claude.json" ]; then
  if [ -f ~/.claude.json ] && [ ! -L ~/.claude.json ]; then
    cp ~/.claude.json "\$SESSION_DIR/claude.json"
  else
    echo '{}' > "\$SESSION_DIR/claude.json"
  fi
fi
ln -sfn "\$SESSION_DIR/claude.json" ~/.claude.json
EOF
}

_wsl_codex_setup_home() {
  local distro="$1"
  local state_dir="$2"
  local placeholder_auth_json="${3:-}"
  _wsl_run_script "$distro" "user" << EOF
CODEX_HOME_DIR="${state_dir}/codex-home"
mkdir -p "\$CODEX_HOME_DIR"
if [ -d ~/.codex ] && [ ! -L ~/.codex ]; then
  cp -a ~/.codex/. "\$CODEX_HOME_DIR/" 2>/dev/null || true
  rm -rf ~/.codex
fi
ln -sfn "\$CODEX_HOME_DIR" ~/.codex
if [ -f "\$CODEX_HOME_DIR/auth.json" ] && grep -q "placeholder-refresh-token-injected-by-proxy\|\\.placeholder\"" "\$CODEX_HOME_DIR/auth.json"; then
  rm -f "\$CODEX_HOME_DIR/auth.json"
fi
if [ ! -f "\$CODEX_HOME_DIR/config.toml" ]; then
  printf 'sandbox_mode = "danger-full-access"\napproval_policy = "never"\n' \\
    > "\$CODEX_HOME_DIR/config.toml"
fi
EOF
  if [ -n "$placeholder_auth_json" ]; then
    # Write auth.json directly via outer bash since state_dir is a Windows path
    mkdir -p "${state_dir}/codex-home"
    printf '%s\n' "$placeholder_auth_json" > "${state_dir}/codex-home/auth.json"
    chmod 600 "${state_dir}/codex-home/auth.json" 2>/dev/null || true
  fi
}

_wsl_copilot_setup_home() {
  local distro="$1"
  local state_dir="$2"
  _wsl_run_script "$distro" "user" << EOF
COPILOT_HOME_DIR="${state_dir}/copilot-home"
mkdir -p "\$COPILOT_HOME_DIR"
if [ -d ~/.copilot ] && [ ! -L ~/.copilot ]; then
  cp -a ~/.copilot/. "\$COPILOT_HOME_DIR/" 2>/dev/null || true
  rm -rf ~/.copilot
fi
ln -sfn "\$COPILOT_HOME_DIR" ~/.copilot
CONFIG="\$COPILOT_HOME_DIR/config.json"
[ ! -f "\$CONFIG" ] && echo '{}' > "\$CONFIG"
jq ".trusted_folders = [\"/\"]" "\$CONFIG" > "\$CONFIG.tmp" && mv "\$CONFIG.tmp" "\$CONFIG"
EOF
}

_wsl_ensure_onboarding_config() {
  local distro="$1"
  local host_dir="$2"
  _wsl_run_script "$distro" "user" << EOF
CONFIG="\$HOME/.claude.json"
SETTINGS="\$HOME/.claude/settings.json"
mkdir -p "\$HOME/.claude"
[ ! -f "\$SETTINGS" ] && echo '{}' > "\$SETTINGS"
jq ".hasCompletedOnboarding = true | .skipDangerousModePermissionPrompt = true" \\
  "\$SETTINGS" > "\$SETTINGS.tmp" && mv "\$SETTINGS.tmp" "\$SETTINGS"
[ ! -f "\$CONFIG" ] && echo '{}' > "\$CONFIG"
jq --arg project_path "${host_dir}" \\
  '.hasCompletedOnboarding = true
   | .lastOnboardingVersion = (.lastOnboardingVersion // "vm")
   | .effortCalloutDismissed = true
   | .projects = (.projects // {})
   | .projects[\$project_path] = ((.projects[\$project_path] // {}) + {hasTrustDialogAccepted: true})' \\
  "\$CONFIG" > "\$CONFIG.tmp" && mv "\$CONFIG.tmp" "\$CONFIG"
EOF
}

_wsl_inject_git_credentials() {
  local distro="$1"
  local git_name git_email
  git_name=$(git config user.name 2>/dev/null || echo "")
  git_email=$(git config user.email 2>/dev/null || echo "")

  _wsl_run_script "$distro" "user" << EOF
git config --global credential.helper store
mkdir -p \$HOME
echo 'https://x-access-token:placeholder@github.com' > \$HOME/.git-credentials
chmod 600 \$HOME/.git-credentials
git config --global url."https://github.com/".insteadOf "git@github.com:"
${git_name:+git config --global user.name "${git_name}"}
${git_email:+git config --global user.email "${git_email}"}
EOF
}

_wsl_inject_gh_credentials() {
  local distro="$1"
  _wsl_run_script "$distro" "user" << 'EOF'
mkdir -p "$HOME/.config/gh"
cat > "$HOME/.config/gh/config.yml" << 'CONFIG'
version: "1"
git_protocol: https
CONFIG
cat > "$HOME/.config/gh/hosts.yml" << 'HOSTS'
github.com:
    oauth_token: placeholder-token-injected-by-proxy
    user: x-access-token
    git_protocol: https
HOSTS
EOF
}

_wsl_write_instructions() {
  local distro="$1"
  local repos_json="$2"

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

  # Write instructions to a Windows-accessible temp file (avoids backtick escaping in heredoc)
  local instructions_file; instructions_file="$(_wsl_win_home)/AppData/Local/Temp/agent-vm-instructions-$$.md"
  cat > "$instructions_file" << 'INSTREOF'

# Git access

Git push and pull to GitHub work out of the box for this repository.
You can use standard git commands:

    git push origin main
    git push origin HEAD:my-branch
    git pull origin main

The `gh` CLI also works and is pre-authenticated. You can use it for
pull requests, issues, and other GitHub operations:

    gh pr list
    gh pr create --title "..." --body "..."
    gh issue list

INSTREOF
  printf '%s\n' "$repo_list" >> "$instructions_file"

  _wsl_run_script "$distro" "user" << EOF
mkdir -p \$HOME/.claude
cat "${instructions_file}" >> \$HOME/.claude/CLAUDE.md
rm -f "${instructions_file}"
EOF
}

_wsl_setup_mitmproxy() {
  local distro="$1"

  _wsl_run_script "$distro" "user" << 'EOF'
if [ ! -f ~/.mitmproxy/mitmproxy-ca-cert.pem ]; then
  timeout 2 mitmdump --listen-port 0 2>/dev/null || true
fi
sudo cp ~/.mitmproxy/mitmproxy-ca-cert.pem /usr/local/share/ca-certificates/mitmproxy-ca.crt
sudo update-ca-certificates
git config --global http.sslCAInfo /etc/ssl/certs/ca-certificates.crt
EOF

  _wsl_run_script "$distro" "root" << 'EOF'
tee /etc/profile.d/credential-proxy.sh > /dev/null << 'PROXYEOF'
export HTTPS_PROXY=http://127.0.0.1:8080
export HTTP_PROXY=http://127.0.0.1:8080
export https_proxy=http://127.0.0.1:8080
export http_proxy=http://127.0.0.1:8080
export NODE_EXTRA_CA_CERTS=/etc/ssl/certs/ca-certificates.crt
PROXYEOF
chmod 644 /etc/profile.d/credential-proxy.sh
EOF
}

_wsl_start_mitmproxy() {
  local distro="$1"
  local credential_proxy_port="$2"
  local intercepted_domains="$3"

  # Copy addon script to distro
  wsl.exe -d "$distro" -u user -- bash -c 'mkdir -p ~/.mitmproxy'
  cat "$SCRIPT_DIR/mitmproxy-addon.py" | wsl.exe -d "$distro" -u user -- bash -c 'cat > ~/.mitmproxy/addon.py'

  # In WSL2, all distros share the same network namespace.
  # The credential proxy bound to 127.0.0.1 on the HOST is reachable from here.
  local proxy_host; proxy_host="$(_wsl_credential_proxy_host)"

  _wsl_run_script "$distro" "user" << 'EOF'
cat > /tmp/start-mitmproxy.sh << 'LAUNCHER'
#!/bin/bash
exec mitmdump \
  --listen-port 8080 \
  --set connection_strategy=lazy \
  -s ~/.mitmproxy/addon.py
LAUNCHER
chmod +x /tmp/start-mitmproxy.sh
EOF

  # Start mitmproxy in the background via nohup + setsid.
  # setsid ensures it survives the parent shell exiting.
  # systemd (enabled via wsl.conf) will keep the distro alive.
  _wsl_run_script "$distro" "user" << EOF
nohup setsid env \\
  CREDENTIAL_PROXY_HOST='${proxy_host}' \\
  CREDENTIAL_PROXY_PORT='${credential_proxy_port}' \\
  CREDENTIAL_PROXY_SECRET='${_credential_proxy_secret}' \\
  CREDENTIAL_PROXY_DOMAINS='${intercepted_domains}' \\
  BLOCKED_DOMAINS='datadoghq.com' \\
  /tmp/start-mitmproxy.sh > /tmp/mitmproxy.log 2>&1 &
echo \$! > /tmp/mitmproxy.pid
EOF

  # Wait up to 6 seconds for mitmproxy to be ready
  _wsl_run_script "$distro" "user" << 'EOF'
for i in $(seq 1 30); do
  if bash -c "echo >/dev/tcp/127.0.0.1/8080" 2>/dev/null; then
    echo "mitmproxy ready"
    exit 0
  fi
  sleep 0.2
done
echo "ERROR: mitmproxy failed to start. Logs:" >&2
cat /tmp/mitmproxy.log >&2
exit 1
EOF
}

_wsl_opencode_setup_config() {
  local distro="$1"
  local state_dir="$2"
  local config_dir="${state_dir}/opencode-config"
  mkdir -p "$config_dir"
  python3 -c "
import json
config = {
    '\$schema': 'https://opencode.ai/config.json',
    'disabled_providers': ['anthropic'],
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

_wsl_opencode_setup_auth() {
  local distro="$1"
  local _anthropic_auth_json=""
  local _openai_api_auth_json=""
  local _openai_oauth_auth_json="${_opencode_openai_auth_json:-}"

  if [ -n "${_anthropic_token:-}" ]; then
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
auth = {}
if sys.argv[1]: auth['anthropic'] = json.loads(sys.argv[1])
if sys.argv[3]:   auth['openai'] = json.loads(sys.argv[3])
elif sys.argv[2]: auth['openai'] = json.loads(sys.argv[2])
if sys.argv[4]:   auth['github-copilot'] = json.loads(sys.argv[4])
print(json.dumps(auth, indent=2))
" "$_anthropic_auth_json" "$_openai_api_auth_json" "$_openai_oauth_auth_json" "$_copilot_auth_json")

  _wsl_run_script "$distro" "user" << EOF
mkdir -p ~/.local/share/opencode
cat > ~/.local/share/opencode/auth.json << 'AUTHEOF'
${auth_json}
AUTHEOF
chmod 600 ~/.local/share/opencode/auth.json
EOF
}

_wsl_opencode_setup_session_persistence() {
  local distro="$1"
  local state_dir="$2"
  _wsl_run_script "$distro" "user" << EOF
SESSION_DIR="${state_dir}/opencode-sessions"
mkdir -p "\$SESSION_DIR" ~/.local/share
if [ -d ~/.local/share/opencode ] && [ ! -L ~/.local/share/opencode ]; then
  cp -a ~/.local/share/opencode/. "\$SESSION_DIR/" 2>/dev/null || true
  rm -rf ~/.local/share/opencode
fi
mkdir -p "\$SESSION_DIR"
ln -sfn "\$SESSION_DIR" ~/.local/share/opencode
EOF
}

# ── _agent_vm_run: WSL2 implementation ───────────────────────────────────────

_agent_vm_run() {
  local agent="$1"
  shift
  local use_github=true
  local extra_mounts=()
  local args=()

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --help|-h)
        echo "Usage: agent-vm ${agent} [options] [agent-args...]"
        echo "Options: --no-git, --mount DIR"
        echo "         (--usb, --memory, --max-memory are ignored on WSL2)"
        return 0
        ;;
      --no-git) use_github=false; shift ;;
      --mount) extra_mounts+=("$2"); shift 2 ;;
      --mount=*) extra_mounts+=("${1#*=}"); shift ;;
      --usb|--usb=*)
        echo "Warning: --usb is not supported on WSL2" >&2
        [[ "$1" == --usb ]] && shift 2 || shift
        ;;
      --memory|--memory=*|--max-memory|--max-memory=*)
        echo "Warning: --memory/--max-memory (balloon) not supported on WSL2, ignored" >&2
        [[ "$1" == *=* ]] && shift || shift 2
        ;;
      *) args+=("$1"); shift ;;
    esac
  done

  local distro="${agent}-$(basename "$(pwd)" | tr -cs 'a-zA-Z0-9' '-' | sed 's/^-//;s/-$//')-$$"
  local host_dir="$(pwd)"
  local state_dir; state_dir="$(_agent_vm_project_state_dir "$host_dir")"
  local template_tar; template_tar="$(_wsl_template_tar)"

  if [ ! -f "$template_tar" ]; then
    echo "Error: WSL2 template not found at $template_tar" >&2
    echo "Run 'agent-vm setup' first." >&2
    return 1
  fi

  # ── Cleanup trap ──────────────────────────────────────────────────────────
  local _cleanup_done=false
  _wsl_run_cleanup() {
    $_cleanup_done && return
    _cleanup_done=true
    echo "Cleaning up WSL2 distro '$distro'..."
    [ -n "${_credential_proxy_pid:-}" ] && kill "$_credential_proxy_pid" 2>/dev/null || true
    wsl.exe --terminate "$distro" 2>/dev/null || true
    wsl.exe --unregister "$distro" 2>/dev/null || true
    rm -rf "$(_wsl_instances_dir)/$distro" 2>/dev/null || true
    _wsl_unmount_shared "$distro"
    [ -n "${security_snapshot:-}" ] && rm -f "$security_snapshot" "${security_snapshot}.git-config"
  }
  trap _wsl_run_cleanup EXIT INT TERM

  # ── Token acquisition (same logic as claude-vm.sh) ────────────────────────
  local _anthropic_token="${CLAUDE_VM_PROXY_ACCESS_TOKEN:-}"
  local _openai_token="${OPENAI_API_KEY:-}"
  local _copilot_token=""
  local _codex_proxy_token="${_openai_token:-}"
  local _codex_placeholder_auth_json=""
  local _opencode_openai_auth_json=""

  if [ "$agent" = "codex" ] && [ -z "$_openai_token" ]; then
    if _codex_vm_prepare_host_auth 2>/dev/null; then
      echo "Using host Codex ChatGPT auth via credential proxy."
    else
      _codex_proxy_token=""
      _codex_placeholder_auth_json=""
      echo "Host Codex auth not available; falling back to VM-local Codex login."
    fi
  elif [ "$agent" = "opencode" ] && [ -z "$_openai_token" ]; then
    if _opencode_vm_prepare_host_auth 2>/dev/null; then
      echo "Using host Codex ChatGPT auth via native OpenCode OAuth."
    else
      _opencode_openai_auth_json=""
      echo "Host Codex auth not available; OpenCode will use its configured providers."
    fi
  fi

  # ── GitHub tokens ─────────────────────────────────────────────────────────
  _claude_vm_github_repos_json='{}'
  _claude_vm_github_token=""
  if $use_github; then
    _claude_vm_get_github_token "$host_dir" "${extra_mounts[@]}" || echo "Continuing without GitHub..."
    _claude_vm_get_copilot_token
  fi

  # ── Credential proxy ──────────────────────────────────────────────────────
  local _credential_rules
  _credential_rules=$(_claude_vm_build_credential_rules \
    "$_anthropic_token" "${_codex_proxy_token:-}" "$_claude_vm_github_repos_json" "${_copilot_token:-}")
  local _credential_proxy_port=""
  local _credential_proxy_pid=""
  local _credential_proxy_secret=""
  if [ "$_credential_rules" != "[]" ]; then
    _claude_vm_start_credential_proxy "$_credential_rules" || {
      _wsl_run_cleanup; trap - EXIT INT TERM; return 1
    }
  fi

  # ── Security snapshot ─────────────────────────────────────────────────────
  local security_snapshot; security_snapshot="$(mktemp)"
  _claude_vm_security_snapshot "$host_dir" "$security_snapshot"
  mkdir -p "${host_dir}/.git/hooks"

  # ── Import and start the WSL2 distro ──────────────────────────────────────
  echo "Starting WSL2 distro '$distro'..."
  _wsl_import_template "$distro" || {
    echo "Error: Failed to import WSL2 template." >&2
    _wsl_run_cleanup; trap - EXIT INT TERM; return 1
  }

  # ── Mount project and state directories ───────────────────────────────────
  echo "Mounting project directory..."
  _wsl_mount_dir "$distro" "$host_dir" "$host_dir"
  mkdir -p "$state_dir"
  _wsl_mount_dir "$distro" "$state_dir" "$state_dir"
  local _m
  for _m in "${extra_mounts[@]}"; do
    _m="$(realpath "$_m" 2>/dev/null)" || continue
    _wsl_mount_dir "$distro" "$_m" "$_m"
  done

  # ── Post-boot agent configuration ─────────────────────────────────────────
  _claude_vm_post_boot_setup_wsl
  _claude_vm_print_config

  # ── Update agent to latest ────────────────────────────────────────────────
  if [ "$agent" = "opencode" ]; then
    echo "Updating OpenCode..."
    wsl.exe -d "$distro" -u user -- bash -lc 'opencode update 2>/dev/null || true'
  elif [ "$agent" = "codex" ]; then
    echo "Updating Codex..."
    wsl.exe -d "$distro" -u user -- bash -lc \
      '(curl -fsSL https://github.com/openai/codex/releases/latest/download/install.sh | sh) >/dev/null 2>&1 || true'
  elif [ "$agent" = "copilot" ]; then
    echo "Updating Copilot CLI..."
    wsl.exe -d "$distro" -u user -- bash -lc \
      '(curl -fsSL https://gh.io/copilot-install | sed "s|/dev/tty|/dev/stdin|g" | bash < /dev/null) >/dev/null 2>&1 || true'
  else
    echo "Updating Claude Code..."
    wsl.exe -d "$distro" -u user -- bash -lc 'claude update --yes 2>/dev/null || true'
  fi

  # ── Run the agent ─────────────────────────────────────────────────────────
  if [ "$agent" = "opencode" ]; then
    CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
      wsl.exe -d "$distro" -u user -- bash -lc \
      "cd '$host_dir' && OPENCODE_CONFIG='${state_dir}/opencode-config/opencode.json' opencode $(printf '%q ' "${args[@]}")"
  elif [ "$agent" = "codex" ]; then
    local codex_env_prefix=""
    [ -n "$_openai_token" ] && codex_env_prefix="OPENAI_API_KEY=dummy-key-auth-handled-by-proxy "
    CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
      wsl.exe -d "$distro" -u user -- bash -lc \
      "cd '$host_dir' && ${codex_env_prefix}codex --dangerously-bypass-approvals-and-sandbox $(printf '%q ' "${args[@]}")"
  elif [ "$agent" = "copilot" ]; then
    if ! wsl.exe -d "$distro" -u user -- bash -c 'command -v copilot &>/dev/null'; then
      echo "Note: Copilot CLI not installed in template. Run 'agent-vm setup' to add it." >&2
      _wsl_run_cleanup; trap - EXIT INT TERM; return 1
    fi
    CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
      wsl.exe -d "$distro" -u user -- bash -lc \
      "cd '$host_dir' && copilot --yolo --model claude-opus-4.6 $(printf '%q ' "${args[@]}")"
  else
    local claude_args=("--model" "opus[1m]" "${args[@]}")
    CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
      wsl.exe -d "$distro" -u user -- \
      env IS_SANDBOX=1 ENABLE_LSP_TOOL=1 bash -lc \
      "cd '$host_dir' && claude --dangerously-skip-permissions $(printf '%q ' "${claude_args[@]}")"
  fi

  _claude_vm_security_check "$host_dir" "$security_snapshot"
  _wsl_run_cleanup
  trap - EXIT INT TERM
}

# ── _agent_vm_shell: WSL2 debug shell ────────────────────────────────────────

_agent_vm_shell() {
  local agent=""
  local use_github=true
  local extra_mounts=()

  if [[ $# -gt 0 && "$1" != --* ]]; then
    agent="$1"; shift
  fi

  case "$agent" in
    ""|claude|opencode|codex|copilot) ;;
    *) echo "Error: Unknown agent '$agent'. Use: claude, opencode, codex, or copilot." >&2; return 1 ;;
  esac

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --no-git) use_github=false; shift ;;
      --mount) extra_mounts+=("$2"); shift 2 ;;
      --mount=*) extra_mounts+=("${1#*=}"); shift ;;
      --usb|--usb=*|--memory|--memory=*|--max-memory|--max-memory=*)
        [[ "$1" == *=* ]] && shift || shift 2 ;;
      *) shift ;;
    esac
  done

  local distro="${agent:+${agent}-}debug-$$"
  local host_dir="$(pwd)"
  local state_dir; state_dir="$(_agent_vm_project_state_dir "$host_dir")"
  local template_tar; template_tar="$(_wsl_template_tar)"

  if [ ! -f "$template_tar" ]; then
    echo "Error: WSL2 template not found. Run 'agent-vm setup' first." >&2
    return 1
  fi

  local _cleanup_done=false
  _wsl_shell_cleanup() {
    $_cleanup_done && return
    _cleanup_done=true
    echo "Cleaning up WSL2 distro '$distro'..."
    [ -n "${_credential_proxy_pid:-}" ] && kill "$_credential_proxy_pid" 2>/dev/null || true
    wsl.exe --terminate "$distro" 2>/dev/null || true
    wsl.exe --unregister "$distro" 2>/dev/null || true
    rm -rf "$(_wsl_instances_dir)/$distro" 2>/dev/null || true
    _wsl_unmount_shared "$distro"
    [ -n "${security_snapshot:-}" ] && rm -f "$security_snapshot" "${security_snapshot}.git-config"
  }
  trap _wsl_shell_cleanup EXIT INT TERM

  local _anthropic_token="${CLAUDE_VM_PROXY_ACCESS_TOKEN:-}"
  local _openai_token="${OPENAI_API_KEY:-}"
  local _copilot_token=""
  local _codex_proxy_token="${_openai_token:-}"
  local _codex_placeholder_auth_json=""
  local _opencode_openai_auth_json=""

  if [ -z "$_openai_token" ]; then
    if [ "$agent" = "opencode" ] || [ -z "$agent" ]; then
      _opencode_vm_prepare_host_auth 2>/dev/null && \
        echo "Using host Codex ChatGPT auth via native OpenCode OAuth." || \
        { _opencode_openai_auth_json=""; }
    fi
    if [ "$agent" != "opencode" ]; then
      _codex_vm_prepare_host_auth 2>/dev/null && \
        echo "Using host Codex ChatGPT auth via credential proxy." || \
        { _codex_proxy_token=""; _codex_placeholder_auth_json=""; }
    fi
  fi

  _claude_vm_github_repos_json='{}'
  _claude_vm_github_token=""
  if $use_github; then
    _claude_vm_get_github_token "$host_dir" "${extra_mounts[@]}" || echo "Continuing without GitHub..."
    _claude_vm_get_copilot_token
  fi

  local _credential_rules
  _credential_rules=$(_claude_vm_build_credential_rules \
    "$_anthropic_token" "${_codex_proxy_token:-}" "$_claude_vm_github_repos_json" "${_copilot_token:-}")
  local _credential_proxy_port=""
  local _credential_proxy_pid=""
  local _credential_proxy_secret=""
  if [ "$_credential_rules" != "[]" ]; then
    _claude_vm_start_credential_proxy "$_credential_rules" || {
      _wsl_shell_cleanup; trap - EXIT INT TERM; return 1
    }
  fi

  local security_snapshot; security_snapshot="$(mktemp)"
  _claude_vm_security_snapshot "$host_dir" "$security_snapshot"
  mkdir -p "${host_dir}/.git/hooks"

  echo "Starting WSL2 distro '$distro'..."
  _wsl_import_template "$distro" || {
    echo "Error: Failed to import WSL2 template." >&2
    _wsl_shell_cleanup; trap - EXIT INT TERM; return 1
  }

  echo "Mounting project directory..."
  _wsl_mount_dir "$distro" "$host_dir" "$host_dir"
  mkdir -p "$state_dir"
  _wsl_mount_dir "$distro" "$state_dir" "$state_dir"
  local _m
  for _m in "${extra_mounts[@]}"; do
    _m="$(realpath "$_m" 2>/dev/null)" || continue
    _wsl_mount_dir "$distro" "$_m" "$_m"
  done

  _claude_vm_post_boot_setup_wsl

  echo "Updating agents..."
  wsl.exe -d "$distro" -u user -- bash -lc 'claude update --yes 2>/dev/null || true'
  wsl.exe -d "$distro" -u user -- bash -lc 'opencode update 2>/dev/null || true'
  wsl.exe -d "$distro" -u user -- bash -lc \
    '(curl -fsSL https://github.com/openai/codex/releases/latest/download/install.sh | sh) >/dev/null 2>&1 || true'
  wsl.exe -d "$distro" -u user -- bash -lc \
    '(curl -fsSL https://gh.io/copilot-install | sed "s|/dev/tty|/dev/stdin|g" | bash < /dev/null) >/dev/null 2>&1 || true'

  echo "WSL2 distro: $distro | Dir: $host_dir${agent:+ | Agent: $agent}"
  _claude_vm_print_config
  echo "OpenCode config: ${state_dir}/opencode-config/opencode.json"
  echo "Type 'exit' to stop and delete the WSL2 distro"

  CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
    wsl.exe -d "$distro" -u user -- \
    env ENABLE_LSP_TOOL=1 IS_SANDBOX=1 \
    OPENCODE_CONFIG="${state_dir}/opencode-config/opencode.json" \
    bash -lc "cd '$host_dir' && bash -l"

  _claude_vm_security_check "$host_dir" "$security_snapshot"
  _wsl_shell_cleanup
  trap - EXIT INT TERM
}

# ── Main entry point (overrides the Lima version from claude-vm.sh) ───────────

agent-vm() {
  local subcmd="${1:-}"
  if [ -z "$subcmd" ] || [ "$subcmd" = "--help" ] || [ "$subcmd" = "-h" ]; then
    echo "Usage: agent-vm <command> [options]"
    echo ""
    echo "Run AI coding agents inside sandboxed WSL2 distros (Windows)."
    echo ""
    echo "Commands:"
    echo "  setup              Create the WSL2 distro template (run once)"
    echo "  claude [args]      Run Claude Code in a sandboxed WSL2 distro"
    echo "  opencode [args]    Run OpenCode in a sandboxed WSL2 distro"
    echo "  codex [args]       Run Codex in a sandboxed WSL2 distro"
    echo "  copilot [args]     Run GitHub Copilot CLI in a sandboxed WSL2 distro"
    echo "  shell [agent]      Open a debug shell (optionally pre-configured for an agent)"
    echo ""
    echo "Options (for claude, opencode, codex, copilot, shell):"
    echo "  --no-git           Skip GitHub integration"
    echo "  --mount DIR        Mount additional directory into the WSL2 distro"
    echo ""
    echo "Note: Uses WSL2 instead of Lima (Windows). USB passthrough and balloon"
    echo "      memory control are not supported on WSL2."
    [ -z "$subcmd" ] && return 1
    return 0
  fi
  shift

  case "$subcmd" in
    setup)    _agent_vm_setup "$@" ;;
    claude)   _agent_vm_run claude "$@" ;;
    opencode) _agent_vm_run opencode "$@" ;;
    codex)    _agent_vm_run codex "$@" ;;
    copilot)  _agent_vm_run copilot "$@" ;;
    shell)    _agent_vm_shell "$@" ;;
    memory)
      echo "Error: 'agent-vm memory' (balloon control) is not supported on WSL2." >&2
      return 1
      ;;
    *)
      echo "Error: Unknown command '$subcmd'" >&2
      echo "Run 'agent-vm' for usage." >&2
      return 1
      ;;
  esac
}
