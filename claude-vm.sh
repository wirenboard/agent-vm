#!/usr/bin/env bash
#
# agent-vm: Run AI coding agents inside a sandboxed VM
# Part of https://github.com/sylvinus/agent-vm
#
# Source this file in your shell config:
#   source /path/to/agent-vm/claude-vm.sh
#
# Usage:
#   agent-vm setup            - Create the VM template/distro (run once)
#   agent-vm claude [args]    - Run Claude in a fresh VM (args forwarded to claude)
#   agent-vm opencode [args]  - Run OpenCode in a fresh VM (args forwarded to opencode)
#   agent-vm codex [args]     - Run Codex in a fresh VM (args forwarded to codex)
#   agent-vm copilot [args]   - Run GitHub Copilot CLI in a fresh VM (args forwarded to copilot)
#   agent-vm shell [agent]    - Open a debug shell in a fresh VM
#
# Supports: Lima VMs (macOS/Linux), WSL2 distros (Windows)

CLAUDE_VM_TEMPLATE="claude-template"
AGENT_VM_WSL_TEMPLATE_DISTRO="agent-vm-template"

# Capture script directory at source time — BASH_SOURCE[0] is only reliable
# at the top level in zsh; inside functions it may resolve to empty/cwd.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"

# ── Platform detection ────────────────────────────────────────────────────────

_agent_vm_is_wsl2() {
  [ -f /proc/version ] && grep -qi microsoft /proc/version
}
_AGENT_VM_BACKEND="lima"
_agent_vm_is_wsl2 && _AGENT_VM_BACKEND="wsl2"

_agent_vm_state_root() {
  if [ -n "${AGENT_VM_STATE_DIR:-}" ]; then
    printf '%s\n' "$AGENT_VM_STATE_DIR"
  elif [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    # Use Windows AppData so per-project state dirs live on C: drive,
    # accessible from every WSL2 distro via DrvFs auto-mount.
    local win_home; win_home="$(_wsl_win_home)"
    printf '%s\n' "${win_home}/.local/state/agent-vm"
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

# ── WSL2 management helpers ───────────────────────────────────────────────────
# These functions are only used when _AGENT_VM_BACKEND="wsl2".

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
_wsl_run() {
  local distro="$1"; shift
  wsl.exe -d "$distro" -u user -- bash -lc "$*"
}

# Run a bash command as root inside a WSL2 distro
_wsl_run_root() {
  local distro="$1"; shift
  wsl.exe -d "$distro" -u root -- bash -c "$*"
}

# Run a bash script in a WSL2 distro via a temp file on the Windows drive.
# Avoids binfmt_misc interop problems with heredoc quoting.
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
    # sudo -u (not su -): sudo does not open a PAM session, so it won't create a
    # lingering systemd user session that blocks wsl.exe --terminate / --export.
    local distro_tmp="/tmp/agent-vm-$$-${RANDOM}.sh"
    wsl.exe -d "$distro" -u root -- bash -c "cp '$win_tmp' '$distro_tmp' && chmod 755 '$distro_tmp' && sudo -u '$run_user' env HOME=/home/'$run_user' bash '$distro_tmp'; rc=\$?; rm -f '$distro_tmp'; exit \$rc" < /dev/null
  fi
  local rc=$?
  rm -f "$win_tmp" 2>/dev/null || true
  return $rc
}

# Get Windows path from a WSL path (forward-slash form for wsl.exe --export).
_wsl_win_path() {
  wslpath -m "$1" 2>/dev/null || echo "$1"
}

# Get the Windows user's home directory as a WSL2 /mnt/c/... path.
_wsl_win_home() {
  local win_home; win_home="$(cmd.exe /C "echo %USERPROFILE%" 2>/dev/null | tr -d '\r\n')"
  if [ -n "$win_home" ]; then
    wslpath -u "$win_home" 2>/dev/null || echo "/mnt/c/Users/${USER}"
  else
    echo "/mnt/c/Users/${USER}"
  fi
}

# Where the template tar and per-run instances live (Windows-native C: drive).
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

# Import template tar as a new named distro (equivalent of limactl clone).
_wsl_import_template() {
  local distro="$1"
  local instance_dir; instance_dir="$(_wsl_instances_dir)/$distro"
  mkdir -p "$instance_dir"
  local win_instance_dir; win_instance_dir="$(_wsl_win_path "$instance_dir")"
  local win_template_tar; win_template_tar="$(_wsl_win_path "$(_wsl_template_tar)")"
  wsl.exe --import "$distro" "$win_instance_dir" "$win_template_tar" --version 2
}

# Shared tmpfs mount point root for cross-distro directory sharing.
_wsl_shared_mnt_root() {
  echo "/mnt/wsl/agent-vm"
}

# Mount a HOST distro directory into a running AGENT distro via /mnt/wsl/ bind mount.
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
  local shared_path; shared_path="$(_wsl_shared_mnt_root)/${distro}${host_path}"
  local bind_opts="--bind"
  if [ "$readonly" = "true" ]; then
    bind_opts="--bind -o ro"
  fi

  sudo mkdir -p "$shared_path"
  sudo mount $bind_opts "$host_path" "$shared_path" 2>/dev/null || {
    mkdir -p "$shared_path"
    mount $bind_opts "$host_path" "$shared_path" 2>/dev/null || true
  }

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
    mount | grep "$shared_root" | awk '{print $3}' | sort -r | while read -r mnt; do
      sudo umount "$mnt" 2>/dev/null || true
    done
    rm -rf "$shared_root" 2>/dev/null || true
  fi
}

# In WSL2 all distros share the same network namespace, so the credential
# proxy bound on localhost is reachable from the agent distro.
_wsl_credential_proxy_host() {
  echo "localhost"
}

# Acquire a Debian 13 base rootfs tar for WSL2 import.
# Tries three methods: Docker, existing Debian/Ubuntu distro, debootstrap.
_wsl_get_debian_base() {
  local output_tar="$1"

  # Method 1: Docker
  if command -v docker &>/dev/null && docker info &>/dev/null 2>&1; then
    echo "  Building Debian 13 rootfs via Docker..."
    local cid
    cid=$(docker create debian:trixie)
    docker export "$cid" > "$output_tar"
    docker rm "$cid" &>/dev/null
    echo "  Debian 13 base image created."
    return 0
  fi

  # Method 2: export an existing Debian/Ubuntu WSL2 distro.
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

  # Method 3: debootstrap
  if ! command -v debootstrap &>/dev/null && command -v apt-get &>/dev/null; then
    echo "  Installing debootstrap..."
    sudo apt-get install -y -qq debootstrap 2>/dev/null || true
  fi
  if command -v debootstrap &>/dev/null; then
    echo "  Building Debian 13 rootfs via debootstrap..."
    local tmpdir; tmpdir="$(mktemp -d)"
    sudo debootstrap --arch=amd64 trixie "$tmpdir" https://deb.debian.org/debian
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

# ── Setup: WSL2 template distro ───────────────────────────────────────────────

_wsl_agent_vm_setup() {
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

  # Fix root directory permissions (WSL2 exports capture 700; processes see 755 but raw inode is 700)
  wsl.exe -d "$AGENT_VM_WSL_TEMPLATE_DISTRO" -u root -- bash -c 'chmod 755 /'

  echo "Configuring base system..."
  wsl.exe -d "$AGENT_VM_WSL_TEMPLATE_DISTRO" -u root -- bash -c '
    groupadd -g 1000 user 2>/dev/null || true
    id user &>/dev/null || useradd -m -s /bin/bash -u 1000 -g 1000 user
    apt-get install -y sudo 2>/dev/null || true
    echo "user ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/user
    chmod 440 /etc/sudoers.d/user
    cat > /etc/wsl.conf << '"'"'EOF'"'"'
[user]
default=user
[boot]
systemd=true
EOF
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

  _claude_vm_install_host_proxy_ca "$AGENT_VM_WSL_TEMPLATE_DISTRO"

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

  # Write PATH additions to .profile (not .bashrc which has an early-return for non-interactive).
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
  # Kill user processes before terminating (defensive; sudo -u doesn't create systemd sessions)
  wsl.exe -d "$AGENT_VM_WSL_TEMPLATE_DISTRO" -u root -- bash -c 'pkill -u user 2>/dev/null; loginctl kill-user user 2>/dev/null; true' < /dev/null 2>/dev/null || true
  wsl.exe --terminate "$AGENT_VM_WSL_TEMPLATE_DISTRO" 2>/dev/null || true

  # --terminate only REQUESTS shutdown; with systemd=true the distro can take
  # 15-90 seconds to fully stop. Poll until "Stopped" before export.
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
  for _i in 1 2 3 4 5; do
    if wsl.exe --unregister "$AGENT_VM_WSL_TEMPLATE_DISTRO" 2>/dev/null; then
      break
    fi
    sleep 2
  done
  rm -rf "$setup_dir" 2>/dev/null || true

  echo "Template ready. Run 'agent-vm claude', 'agent-vm opencode', or 'agent-vm codex' in any project directory."
}

# ── Setup: Lima VM template ───────────────────────────────────────────────────

_agent_vm_setup() {
  # Dispatch to WSL2 implementation when running on Windows
  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    _wsl_agent_vm_setup "$@"
    return $?
  fi

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
  limactl shell "$CLAUDE_VM_TEMPLATE" bash -lc 'curl -fsSL https://gh.io/copilot-install | sed "s|/dev/tty|/dev/stdin|g" | bash < /dev/null'

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
  [ -f "$host_ca" ] || return 0

  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    echo "Installing host MITM CA into WSL2 distro trust store..."
    cat "$host_ca" | wsl.exe -d "$vm_name" -u root -- tee /usr/local/share/ca-certificates/host-mitmproxy-ca.crt > /dev/null
    wsl.exe -d "$vm_name" -u root -- update-ca-certificates
    wsl.exe -d "$vm_name" -u user -- git config --global http.sslCAInfo /etc/ssl/certs/ca-certificates.crt
    return
  fi

  echo "Installing host MITM CA into VM trust store..."
  cat "$host_ca" | limactl shell "$vm_name" sudo tee /usr/local/share/ca-certificates/host-mitmproxy-ca.crt > /dev/null
  limactl shell "$vm_name" sudo update-ca-certificates
  limactl shell "$vm_name" git config --global http.sslCAInfo /etc/ssl/certs/ca-certificates.crt
}

_claude_vm_build_credential_rules() {
  # Build CREDENTIAL_PROXY_RULES JSON from tokens.
  # Args: anthropic_token openai_token github_repos_json copilot_token
  # github_repos_json: '{"owner/repo": "token", ...}' — per-repo tokens
  local anthropic_token="$1"
  local openai_token="$2"
  local github_repos_json="$3"
  local copilot_token="$4"

  python3 -c "
import base64, json, sys
rules = []
anthropic_token = sys.argv[1]
openai_token = sys.argv[2]
repos = json.loads(sys.argv[3]) if sys.argv[3] else {}
copilot_token = sys.argv[4]
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
" "$anthropic_token" "$openai_token" "$github_repos_json" "$copilot_token"
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

  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    _wsl_run_script "$vm_name" "user" << 'EOF'
if [ ! -f ~/.mitmproxy/mitmproxy-ca-cert.pem ]; then
  timeout 2 mitmdump --listen-port 0 2>/dev/null || true
fi
sudo cp ~/.mitmproxy/mitmproxy-ca-cert.pem /usr/local/share/ca-certificates/mitmproxy-ca.crt
sudo update-ca-certificates
git config --global http.sslCAInfo /etc/ssl/certs/ca-certificates.crt
EOF
    _wsl_run_script "$vm_name" "root" << 'EOF'
tee /etc/profile.d/credential-proxy.sh > /dev/null << 'PROXYEOF'
export HTTPS_PROXY=http://127.0.0.1:8080
export HTTP_PROXY=http://127.0.0.1:8080
export https_proxy=http://127.0.0.1:8080
export http_proxy=http://127.0.0.1:8080
export NODE_EXTRA_CA_CERTS=/etc/ssl/certs/ca-certificates.crt
PROXYEOF
chmod 644 /etc/profile.d/credential-proxy.sh
EOF
    return
  fi

  # Generate mitmproxy CA cert and install in system trust store
  limactl shell "$vm_name" bash -c '
    if [ ! -f ~/.mitmproxy/mitmproxy-ca-cert.pem ]; then
      timeout 2 mitmdump --listen-port 0 2>/dev/null || true
    fi
    sudo cp ~/.mitmproxy/mitmproxy-ca-cert.pem /usr/local/share/ca-certificates/mitmproxy-ca.crt
    sudo update-ca-certificates
    git config --global http.sslCAInfo /etc/ssl/certs/ca-certificates.crt
  '

  # Set HTTPS_PROXY env var for all processes via profile.d
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
  local intercepted_domains="$3"

  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    local proxy_host; proxy_host="$(_wsl_credential_proxy_host)"
    wsl.exe -d "$vm_name" -u user -- bash -c 'mkdir -p ~/.mitmproxy'
    cat "$SCRIPT_DIR/mitmproxy-addon.py" | wsl.exe -d "$vm_name" -u user -- bash -c 'cat > ~/.mitmproxy/addon.py'
    _wsl_run_script "$vm_name" "user" << 'EOF'
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
    _wsl_run_script "$vm_name" "user" << EOF
nohup setsid env \\
  CREDENTIAL_PROXY_HOST='${proxy_host}' \\
  CREDENTIAL_PROXY_PORT='${credential_proxy_port}' \\
  CREDENTIAL_PROXY_SECRET='${_credential_proxy_secret}' \\
  CREDENTIAL_PROXY_DOMAINS='${intercepted_domains}' \\
  BLOCKED_DOMAINS='datadoghq.com' \\
  /tmp/start-mitmproxy.sh > /tmp/mitmproxy.log 2>&1 &
echo \$! > /tmp/mitmproxy.pid
EOF
    _wsl_run_script "$vm_name" "user" << 'EOF'
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
    return
  fi

  # Lima: copy addon script and start via systemd-run
  limactl shell "$vm_name" bash -c 'mkdir -p ~/.mitmproxy'
  cat "$SCRIPT_DIR/mitmproxy-addon.py" | limactl shell "$vm_name" bash -c 'cat > ~/.mitmproxy/addon.py'

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

  limactl shell "$vm_name" bash -c "
    systemd-run --user --unit=mitmproxy \
      --setenv=CREDENTIAL_PROXY_HOST=host.lima.internal \
      --setenv=CREDENTIAL_PROXY_PORT=$credential_proxy_port \
      --setenv=CREDENTIAL_PROXY_SECRET='$_credential_proxy_secret' \
      --setenv=CREDENTIAL_PROXY_DOMAINS='$intercepted_domains' \
      --setenv=BLOCKED_DOMAINS='datadoghq.com' \
      /tmp/start-mitmproxy.sh
  "

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
  local git_name git_email
  git_name=$(git config user.name 2>/dev/null || echo "")
  git_email=$(git config user.email 2>/dev/null || echo "")

  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    _wsl_run_script "$vm_name" "user" << EOF
git config --global credential.helper store
mkdir -p \$HOME
echo 'https://x-access-token:placeholder@github.com' > \$HOME/.git-credentials
chmod 600 \$HOME/.git-credentials
git config --global url."https://github.com/".insteadOf "git@github.com:"
${git_name:+git config --global user.name "${git_name}"}
${git_email:+git config --global user.email "${git_email}"}
EOF
    return
  fi

  limactl shell "$vm_name" bash -c "
    git config --global credential.helper store
    mkdir -p \$HOME
    echo 'https://x-access-token:placeholder@github.com' > \$HOME/.git-credentials
    chmod 600 \$HOME/.git-credentials
    git config --global url.\"https://github.com/\".insteadOf \"git@github.com:\"
    ${git_name:+git config --global user.name \"$git_name\"}
    ${git_email:+git config --global user.email \"$git_email\"}
  "
}

_claude_vm_inject_gh_credentials() {
  local vm_name="$1"

  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    _wsl_run_script "$vm_name" "user" << 'EOF'
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
    return
  fi

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

  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    # Write instructions to a Windows-accessible temp file (avoids heredoc escaping issues)
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
    _wsl_run_script "$vm_name" "user" << EOF
mkdir -p \$HOME/.claude
cat "${instructions_file}" >> \$HOME/.claude/CLAUDE.md
rm -f "${instructions_file}"
EOF
    return
  fi

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

  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    _wsl_run_script "$vm_name" "user" << EOF
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
    return
  fi

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
  #   _anthropic_token, _openai_token, _codex_proxy_token, _copilot_token,
  #   _codex_placeholder_auth_json, _credential_rules, _credential_proxy_port,
  #   _credential_proxy_secret, _claude_vm_github_repos_json
  # Sets: _oauth_token, _codex_home, _intercepted_domains

  _claude_vm_inject_clipboard_shim "$vm_name" "$state_dir"
  _oauth_token=""
  _codex_home=""

  if [ "$agent" = "claude" ] || [ "$agent" = "opencode" ] || [ -z "$agent" ]; then
    # Write a placeholder oauth token so Claude Code attempts API requests.
    # The real auth header is injected by the host credential proxy —
    # never expose the actual token inside the VM.
    _oauth_token="${_anthropic_token:+placeholder}"
    _oauth_token="${_oauth_token:-${AI_HTTPS_PROXY:+placeholder}}"
    _claude_vm_write_oauth_token "$vm_name" "$_oauth_token"
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
      if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
        wsl.exe -d "$vm_name" -u user -- bash -c \
          'ln -sf "$HOME/.claude/CLAUDE.md" "$HOME/.codex/AGENTS.md"'
      else
        limactl shell "$vm_name" bash -c \
          'ln -sf "$HOME/.claude/CLAUDE.md" "$HOME/.codex/AGENTS.md"'
      fi
    fi

    # Copilot CLI reads ~/.copilot/copilot-instructions.md
    if [ "$agent" = "copilot" ] || [ -z "$agent" ]; then
      if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
        wsl.exe -d "$vm_name" -u user -- bash -c \
          'mkdir -p "$HOME/.copilot" && ln -sf "$HOME/.claude/CLAUDE.md" "$HOME/.copilot/copilot-instructions.md"'
      else
        limactl shell "$vm_name" bash -c \
          'mkdir -p "$HOME/.copilot" && ln -sf "$HOME/.claude/CLAUDE.md" "$HOME/.copilot/copilot-instructions.md"'
      fi
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
    if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
      wsl.exe -d "$vm_name" -u user -- bash -l < "${host_dir}/.claude-vm.runtime.sh"
    else
      limactl shell --workdir "$host_dir" "$vm_name" bash -l < "${host_dir}/.claude-vm.runtime.sh"
    fi
  fi
}

_claude_vm_print_config() {
  # Print proxy/credential configuration summary.
  # Uses variables from the calling function's scope.
  if [ -n "$_credential_proxy_port" ]; then
    local _proxy_host
    if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
      _proxy_host="localhost"
    else
      _proxy_host="host.lima.internal"
    fi
    echo "Credential proxy: http://${_proxy_host}:${_credential_proxy_port}"
    echo "Intercepted domains: $_intercepted_domains"
  fi
  if [ -n "$_oauth_token" ]; then
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
  if [ -n "${AI_SSL_CERT_FILE:-}" ]; then
    echo "AI SSL cert file: $AI_SSL_CERT_FILE"
  fi
}

_claude_vm_write_oauth_token() {
  local vm_name="$1"
  local token="$2"
  [ -z "$token" ] && return 0
  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    _wsl_run_script "$vm_name" "root" << EOF
tee /etc/profile.d/claude-oauth.sh > /dev/null << 'TOKENEOF'
export CLAUDE_CODE_OAUTH_TOKEN=${token}
TOKENEOF
chmod 644 /etc/profile.d/claude-oauth.sh
EOF
    return
  fi
  limactl shell "$vm_name" bash -c "
    sudo tee /etc/profile.d/claude-oauth.sh > /dev/null <<EOF
export CLAUDE_CODE_OAUTH_TOKEN=$token
EOF
    sudo chmod 644 /etc/profile.d/claude-oauth.sh
  "
}

_codex_vm_write_api_key() {
  local vm_name="$1"
  local token="$2"
  [ -z "$token" ] && return 0
  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    _wsl_run_script "$vm_name" "root" << 'EOF'
tee /etc/profile.d/codex-api-key.sh > /dev/null << 'KEYEOF'
export OPENAI_API_KEY=dummy-key-auth-handled-by-proxy
KEYEOF
chmod 644 /etc/profile.d/codex-api-key.sh
EOF
    return
  fi
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
  [ -z "$token" ] && return 0
  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    _wsl_run_script "$vm_name" "root" << 'EOF'
tee /etc/profile.d/copilot-token.sh > /dev/null << 'TOKEOF'
export COPILOT_GITHUB_TOKEN=placeholder-copilot-token-injected-by-proxy
TOKEOF
chmod 644 /etc/profile.d/copilot-token.sh
EOF
    return
  fi
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
  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    _wsl_run_script "$vm_name" "user" << EOF
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
    return
  fi
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
  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    _wsl_run_script "$vm_name" "user" << EOF
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
      mkdir -p "${state_dir}/codex-home"
      printf '%s\n' "$placeholder_auth_json" > "${state_dir}/codex-home/auth.json"
      chmod 600 "${state_dir}/codex-home/auth.json" 2>/dev/null || true
    fi
    return
  fi
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
  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    _wsl_run_script "$vm_name" "user" << EOF
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
    return
  fi
  limactl shell "$vm_name" bash -c '
    COPILOT_HOME_DIR="'"$state_dir"'/copilot-home"
    mkdir -p "$COPILOT_HOME_DIR"
    if [ -d ~/.copilot ] && [ ! -L ~/.copilot ]; then
      cp -a ~/.copilot/. "$COPILOT_HOME_DIR/" 2>/dev/null || true
      rm -rf ~/.copilot
    fi
    ln -sfn "$COPILOT_HOME_DIR" ~/.copilot
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
  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    _wsl_run_script "$vm_name" "user" << EOF
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
    return
  fi
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

_opencode_vm_setup_auth() {
  local vm_name="$1"
  # Write a valid OpenCode auth.json. For OpenAI, prefer native OAuth auth when
  # host Codex ChatGPT auth is available; otherwise fall back to the proxy-based
  # API-key flow.
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

  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    _wsl_run_script "$vm_name" "user" << EOF
mkdir -p ~/.local/share/opencode
cat > ~/.local/share/opencode/auth.json << 'AUTHEOF'
${auth_json}
AUTHEOF
chmod 600 ~/.local/share/opencode/auth.json
EOF
  else
    limactl shell "$vm_name" bash -c '
      mkdir -p ~/.local/share/opencode
      cat > ~/.local/share/opencode/auth.json << '\''AUTH'\''
'"$auth_json"'
AUTH
      chmod 600 ~/.local/share/opencode/auth.json
    '
  fi
}

_opencode_vm_setup_session_persistence() {
  local vm_name="$1"
  local state_dir="$2"
  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    _wsl_run_script "$vm_name" "user" << EOF
SESSION_DIR="${state_dir}/opencode-sessions"
mkdir -p "\$SESSION_DIR" ~/.local/share
if [ -d ~/.local/share/opencode ] && [ ! -L ~/.local/share/opencode ]; then
  cp -a ~/.local/share/opencode/. "\$SESSION_DIR/" 2>/dev/null || true
  rm -rf ~/.local/share/opencode
fi
mkdir -p "\$SESSION_DIR"
ln -sfn "\$SESSION_DIR" ~/.local/share/opencode
EOF
    return
  fi
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
      --help|-h)
        echo "Usage: agent-vm ${agent} [options] [agent-args...]"
        echo "Options: --no-git, --mount DIR, --memory GB, --max-memory GB, --usb DEVICE"
        return 0
        ;;
      --no-git) use_github=false; shift ;;
      --usb)
        if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
          echo "Warning: --usb is not supported on WSL2" >&2; shift 2
        else
          usb_devices+=("$2"); shift 2
        fi
        ;;
      --usb=*)
        if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
          echo "Warning: --usb is not supported on WSL2" >&2; shift
        else
          usb_devices+=("${1#*=}"); shift
        fi
        ;;
      --mount) extra_mounts+=("$2"); shift 2 ;;
      --mount=*) extra_mounts+=("${1#*=}"); shift ;;
      --memory)
        if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
          echo "Warning: --memory not supported on WSL2, ignored" >&2; shift 2
        else
          memory="$2"; shift 2
        fi
        ;;
      --memory=*)
        if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
          echo "Warning: --memory not supported on WSL2, ignored" >&2; shift
        else
          memory="${1#*=}"; shift
        fi
        ;;
      --max-memory)
        if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
          echo "Warning: --max-memory not supported on WSL2, ignored" >&2; shift 2
        else
          max_memory="$2"; shift 2
        fi
        ;;
      --max-memory=*)
        if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
          echo "Warning: --max-memory not supported on WSL2, ignored" >&2; shift
        else
          max_memory="${1#*=}"; shift
        fi
        ;;
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

  # Check template exists
  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    if [ ! -f "$(_wsl_template_tar)" ]; then
      echo "Error: WSL2 template not found. Run 'agent-vm setup' first." >&2
      return 1
    fi
  else
    if ! limactl list -q 2>/dev/null | grep -q "^${CLAUDE_VM_TEMPLATE}$"; then
      echo "Error: Template VM not found. Run 'agent-vm setup' first." >&2
      return 1
    fi
  fi

  # Set up cleanup trap before starting any background processes
  local security_snapshot=""
  local _cleanup_done=false
  _claude_vm_cleanup() {
    $_cleanup_done && return
    _cleanup_done=true
    if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
      echo "Cleaning up WSL2 distro '$vm_name'..."
      [ -n "${_credential_proxy_pid:-}" ] && kill "$_credential_proxy_pid" 2>/dev/null || true
      wsl.exe --terminate "$vm_name" 2>/dev/null || true
      wsl.exe --unregister "$vm_name" 2>/dev/null || true
      rm -rf "$(_wsl_instances_dir)/$vm_name" 2>/dev/null || true
      _wsl_unmount_shared "$vm_name"
    else
      echo "Cleaning up VM..."
      [ -n "$_balloon_daemon_pid" ] && kill "$_balloon_daemon_pid" 2>/dev/null
      [ -n "${_credential_proxy_pid:-}" ] && kill "$_credential_proxy_pid" 2>/dev/null
      limactl stop "$vm_name" &>/dev/null
      limactl delete "$vm_name" --force &>/dev/null
      [ -d "$HOME/.lima/$vm_name" ] && rm -rf "$HOME/.lima/$vm_name"
      local port
      for port in "${_usb_sysfs_ports[@]}"; do
        _agent_vm_usb_rebind "$port"
      done
      [ -n "$_usb_qemu_wrapper" ] && rm -f "$_usb_qemu_wrapper"
    fi
    [ -n "$security_snapshot" ] && rm -f "$security_snapshot" "${security_snapshot}.git-config"
  }
  trap _claude_vm_cleanup EXIT INT TERM

  # ── Token acquisition (shared) ────────────────────────────────────────────
  local _anthropic_token="${CLAUDE_VM_PROXY_ACCESS_TOKEN:-}"
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

  _claude_vm_github_repos_json='{}'
  _claude_vm_github_token=""
  if $use_github; then
    _claude_vm_get_github_token "$host_dir" "${extra_mounts[@]}" || echo "Continuing without GitHub..."
    _claude_vm_get_copilot_token
  fi

  local _credential_rules
  _credential_rules=$(_claude_vm_build_credential_rules "$_anthropic_token" "$_codex_proxy_token" "$_claude_vm_github_repos_json" "$_copilot_token")
  local _credential_proxy_port=""
  local _credential_proxy_pid=""
  local _credential_proxy_secret=""
  if [ "$_credential_rules" != "[]" ]; then
    _claude_vm_start_credential_proxy "$_credential_rules" || { _claude_vm_cleanup; trap - EXIT INT TERM; return 1; }
  fi

  security_snapshot="$(mktemp)"
  _claude_vm_security_snapshot "$host_dir" "$security_snapshot"
  mkdir -p "${host_dir}/.git/hooks"

  # ── Start VM / distro (platform-specific) ────────────────────────────────
  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    echo "Starting WSL2 distro '$vm_name'..."
    _wsl_import_template "$vm_name" || {
      echo "Error: Failed to import WSL2 template." >&2
      _claude_vm_cleanup; trap - EXIT INT TERM; return 1
    }
    echo "Mounting project directory..."
    _wsl_mount_dir "$vm_name" "$host_dir" "$host_dir"
    mkdir -p "$state_dir"
    _wsl_mount_dir "$vm_name" "$state_dir" "$state_dir"
    local _m
    for _m in "${extra_mounts[@]}"; do
      _m="$(realpath "$_m" 2>/dev/null)" || continue
      _wsl_mount_dir "$vm_name" "$_m" "$_m"
    done
  else
    # Lima: clone and start
    local _mounts_json="{\"location\":\"${host_dir}\",\"writable\":true},{\"location\":\"${host_dir}/.git/hooks\",\"writable\":false},{\"location\":\"${state_dir}\",\"writable\":true}"
    local _m
    for _m in "${extra_mounts[@]}"; do
      _mounts_json="${_mounts_json},{\"location\":\"$(realpath "$_m")\",\"writable\":true}"
    done
    local _clone_memory_args=()
    [ -n "$memory" ] && _clone_memory_args=(--memory "$memory")

    echo "Starting VM '$vm_name'..."
    limactl clone "$CLAUDE_VM_TEMPLATE" "$vm_name" \
      --set ".mounts=[${_mounts_json}]" \
      --set '.containerd.system=false' \
      --set '.containerd.user=false' \
      "${_clone_memory_args[@]}" \
      --tty=false &>/dev/null

    # USB passthrough
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

    local _has_balloon=false
    if command -v qemu-system-x86_64 &>/dev/null; then
      [ -n "$max_memory" ] && limactl edit "$vm_name" --memory="$max_memory" --tty=false &>/dev/null
      if [ ${#_usb_vidpids[@]} -gt 0 ]; then
        _usb_qemu_wrapper="$(_agent_vm_qemu_wrapper "${_usb_vidpids[@]}")"
      else
        _usb_qemu_wrapper="$(_agent_vm_qemu_wrapper)"
      fi
      if QEMU_SYSTEM_X86_64="$_usb_qemu_wrapper" limactl start "$vm_name" &>/dev/null; then
        _has_balloon=true
      else
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
      if $_has_balloon; then
        local _balloon_daemon_args=()
        [ -n "$max_memory" ] && _balloon_daemon_args+=(--max-memory "${max_memory}G")
        python3 "$SCRIPT_DIR/balloon-daemon.py" "$HOME/.lima/$vm_name/qmp.sock" daemon "${_balloon_daemon_args[@]}" &>/dev/null &
        _balloon_daemon_pid=$!
      fi
    else
      [ -n "$max_memory" ] && echo "Warning: --max-memory ignored without balloon (QEMU not available)" >&2
      [ -n "$memory" ] && limactl edit "$vm_name" --memory="$memory" --tty=false &>/dev/null
      limactl start "$vm_name" &>/dev/null
    fi
  fi

  # ── Post-boot setup (shared, dispatches internally) ───────────────────────
  _claude_vm_post_boot_setup
  _claude_vm_print_config

  # ── Update agent and run (platform-specific) ─────────────────────────────
  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    if [ "$agent" = "opencode" ]; then
      echo "Updating OpenCode..."
      wsl.exe -d "$vm_name" -u user -- bash -lc 'opencode update 2>/dev/null || true'
    elif [ "$agent" = "codex" ]; then
      echo "Updating Codex..."
      wsl.exe -d "$vm_name" -u user -- bash -lc \
        '(curl -fsSL https://github.com/openai/codex/releases/latest/download/install.sh | sh) >/dev/null 2>&1 || true'
    elif [ "$agent" = "copilot" ]; then
      echo "Updating Copilot CLI..."
      wsl.exe -d "$vm_name" -u user -- bash -lc \
        '(curl -fsSL https://gh.io/copilot-install | sed "s|/dev/tty|/dev/stdin|g" | bash < /dev/null) >/dev/null 2>&1 || true'
    else
      echo "Updating Claude Code..."
      wsl.exe -d "$vm_name" -u user -- bash -lc 'claude update --yes 2>/dev/null || true'
    fi

    if [ "$agent" = "opencode" ]; then
      CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
        wsl.exe -d "$vm_name" -u user -- bash -lc \
        "cd '$host_dir' && OPENCODE_CONFIG='${state_dir}/opencode-config/opencode.json' opencode $(printf '%q ' "${args[@]}")"
    elif [ "$agent" = "codex" ]; then
      local codex_env_prefix=""
      [ -n "$_openai_token" ] && codex_env_prefix="OPENAI_API_KEY=dummy-key-auth-handled-by-proxy "
      CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
        wsl.exe -d "$vm_name" -u user -- bash -lc \
        "cd '$host_dir' && ${codex_env_prefix}codex --dangerously-bypass-approvals-and-sandbox $(printf '%q ' "${args[@]}")"
    elif [ "$agent" = "copilot" ]; then
      if ! wsl.exe -d "$vm_name" -u user -- bash -c 'command -v copilot &>/dev/null'; then
        echo "Note: Copilot CLI not installed in template. Run 'agent-vm setup' to add it." >&2
        _claude_vm_cleanup; trap - EXIT INT TERM; return 1
      fi
      CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
        wsl.exe -d "$vm_name" -u user -- bash -lc \
        "cd '$host_dir' && copilot --yolo --model claude-opus-4.6 $(printf '%q ' "${args[@]}")"
    else
      local claude_args=("--model" "opus[1m]" "${args[@]}")
      CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
        wsl.exe -d "$vm_name" -u user -- \
        env IS_SANDBOX=1 ENABLE_LSP_TOOL=1 bash -lc \
        "cd '$host_dir' && claude --dangerously-skip-permissions $(printf '%q ' "${claude_args[@]}")"
    fi
  else
    # Lima agent execution
    if [ "$agent" = "opencode" ]; then
      echo "Updating OpenCode..."
      limactl shell "$vm_name" bash -lc 'opencode update 2>/dev/null || true'
    elif [ "$agent" = "codex" ]; then
      echo "Updating Codex..."
      limactl shell "$vm_name" bash -lc '(curl -fsSL https://github.com/openai/codex/releases/latest/download/install.sh | sh) >/dev/null 2>&1 || true'
    elif [ "$agent" = "copilot" ]; then
      echo "Updating Copilot CLI..."
      limactl shell "$vm_name" bash -lc '(curl -fsSL https://gh.io/copilot-install | sed "s|/dev/tty|/dev/stdin|g" | bash < /dev/null) >/dev/null 2>&1 || true'
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
      [ -n "$_openai_token" ] && codex_env+=(OPENAI_API_KEY=dummy-key-auth-handled-by-proxy)
      CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
        limactl shell --workdir "$host_dir" "$vm_name" \
        env "${codex_env[@]}" \
        codex --dangerously-bypass-approvals-and-sandbox "${args[@]}"
    elif [ "$agent" = "copilot" ]; then
      if ! limactl shell "$vm_name" bash -c 'command -v copilot &>/dev/null'; then
        echo "Note: Copilot CLI is not installed in the VM template. Run 'agent-vm update-template' to add it." >&2
        return 1
      fi
      CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
        limactl shell --workdir "$host_dir" "$vm_name" \
        env copilot --yolo --model claude-opus-4.6 "${args[@]}"
    else
      local claude_args=("--model" "opus[1m]" "${args[@]}")
      CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
        limactl shell --workdir "$host_dir" "$vm_name" \
        env IS_SANDBOX=1 \
        ENABLE_LSP_TOOL=1 \
        claude --dangerously-skip-permissions "${claude_args[@]}"
    fi
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
      --usb|--usb=*|--memory|--memory=*|--max-memory|--max-memory=*)
        if [ "$_AGENT_VM_BACKEND" != "wsl2" ]; then
          case "$1" in
            --usb) usb_devices+=("$2"); shift 2 ;;
            --usb=*) usb_devices+=("${1#*=}"); shift ;;
            --memory) memory="$2"; shift 2 ;;
            --memory=*) memory="${1#*=}"; shift ;;
            --max-memory) max_memory="$2"; shift 2 ;;
            --max-memory=*) max_memory="${1#*=}"; shift ;;
          esac
        else
          [[ "$1" == *=* ]] && shift || shift 2
        fi
        ;;
      --mount) extra_mounts+=("$2"); shift 2 ;;
      --mount=*) extra_mounts+=("${1#*=}"); shift ;;
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

  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    if [ ! -f "$(_wsl_template_tar)" ]; then
      echo "Error: WSL2 template not found. Run 'agent-vm setup' first." >&2
      return 1
    fi
  else
    if ! limactl list -q 2>/dev/null | grep -q "^${CLAUDE_VM_TEMPLATE}$"; then
      echo "Error: Template VM not found. Run 'agent-vm setup' first." >&2
      return 1
    fi
  fi

  local security_snapshot=""
  local _cleanup_done=false
  _claude_vm_shell_cleanup() {
    $_cleanup_done && return
    _cleanup_done=true
    if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
      echo "Cleaning up WSL2 distro '$vm_name'..."
      [ -n "${_credential_proxy_pid:-}" ] && kill "$_credential_proxy_pid" 2>/dev/null || true
      wsl.exe --terminate "$vm_name" 2>/dev/null || true
      wsl.exe --unregister "$vm_name" 2>/dev/null || true
      rm -rf "$(_wsl_instances_dir)/$vm_name" 2>/dev/null || true
      _wsl_unmount_shared "$vm_name"
    else
      echo "Cleaning up VM..."
      [ -n "$_balloon_daemon_pid" ] && kill "$_balloon_daemon_pid" 2>/dev/null
      [ -n "${_credential_proxy_pid:-}" ] && kill "$_credential_proxy_pid" 2>/dev/null
      limactl stop "$vm_name" &>/dev/null
      limactl delete "$vm_name" --force &>/dev/null
      [ -d "$HOME/.lima/$vm_name" ] && rm -rf "$HOME/.lima/$vm_name"
      local port
      for port in "${_usb_sysfs_ports[@]}"; do
        _agent_vm_usb_rebind "$port"
      done
      [ -n "$_usb_qemu_wrapper" ] && rm -f "$_usb_qemu_wrapper"
    fi
    [ -n "$security_snapshot" ] && rm -f "$security_snapshot" "${security_snapshot}.git-config"
  }
  trap _claude_vm_shell_cleanup EXIT INT TERM

  # ── Token acquisition (shared) ────────────────────────────────────────────
  local _anthropic_token="${CLAUDE_VM_PROXY_ACCESS_TOKEN:-}"
  local _openai_token="${OPENAI_API_KEY:-}"
  local _copilot_token=""
  local _codex_proxy_token="${_openai_token:-}"
  local _codex_host_auth_json=""
  local _codex_placeholder_auth_json=""
  local _opencode_host_auth_json=""
  local _opencode_openai_auth_json=""
  if [ -z "$_openai_token" ]; then
    if [ "$agent" = "opencode" ]; then
      _opencode_vm_prepare_host_auth 2>/dev/null && \
        echo "Using host Codex ChatGPT auth via native OpenCode OAuth." || \
        { _opencode_openai_auth_json=""; echo "Host Codex auth not available; OpenCode will use its configured providers."; }
    elif [ -z "$agent" ]; then
      _codex_vm_prepare_host_auth 2>/dev/null && \
        echo "Using host Codex ChatGPT auth via credential proxy." || \
        { _codex_proxy_token=""; _codex_placeholder_auth_json=""; echo "Host Codex auth not available; falling back to VM-local Codex login."; }
      _opencode_vm_prepare_host_auth 2>/dev/null && \
        echo "Using host Codex ChatGPT auth via native OpenCode OAuth." || \
        { _opencode_openai_auth_json=""; }
    else
      _codex_vm_prepare_host_auth 2>/dev/null && \
        echo "Using host Codex ChatGPT auth via credential proxy." || \
        { _codex_proxy_token=""; _codex_placeholder_auth_json=""; echo "Host Codex auth not available; falling back to VM-local Codex login."; }
    fi
  fi

  _claude_vm_github_repos_json='{}'
  _claude_vm_github_token=""
  if $use_github; then
    _claude_vm_get_github_token "$host_dir" "${extra_mounts[@]}" || echo "Continuing without GitHub..."
    _claude_vm_get_copilot_token
  fi

  local _credential_rules
  _credential_rules=$(_claude_vm_build_credential_rules "$_anthropic_token" "$_codex_proxy_token" "$_claude_vm_github_repos_json" "$_copilot_token")
  local _credential_proxy_port=""
  local _credential_proxy_pid=""
  local _credential_proxy_secret=""
  if [ "$_credential_rules" != "[]" ]; then
    _claude_vm_start_credential_proxy "$_credential_rules" || { _claude_vm_shell_cleanup; trap - EXIT INT TERM; return 1; }
  fi

  security_snapshot="$(mktemp)"
  _claude_vm_security_snapshot "$host_dir" "$security_snapshot"
  mkdir -p "${host_dir}/.git/hooks"

  # ── Start VM / distro (platform-specific) ────────────────────────────────
  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    echo "Starting WSL2 distro '$vm_name'..."
    _wsl_import_template "$vm_name" || {
      echo "Error: Failed to import WSL2 template." >&2
      _claude_vm_shell_cleanup; trap - EXIT INT TERM; return 1
    }
    echo "Mounting project directory..."
    _wsl_mount_dir "$vm_name" "$host_dir" "$host_dir"
    mkdir -p "$state_dir"
    _wsl_mount_dir "$vm_name" "$state_dir" "$state_dir"
    local _m
    for _m in "${extra_mounts[@]}"; do
      _m="$(realpath "$_m" 2>/dev/null)" || continue
      _wsl_mount_dir "$vm_name" "$_m" "$_m"
    done
  else
    local _mounts_json="{\"location\":\"${host_dir}\",\"writable\":true},{\"location\":\"${host_dir}/.git/hooks\",\"writable\":false},{\"location\":\"${state_dir}\",\"writable\":true}"
    local _m
    for _m in "${extra_mounts[@]}"; do
      _mounts_json="${_mounts_json},{\"location\":\"$(realpath "$_m")\",\"writable\":true}"
    done
    local _clone_memory_args=()
    [ -n "$memory" ] && _clone_memory_args=(--memory "$memory")

    limactl clone "$CLAUDE_VM_TEMPLATE" "$vm_name" \
      --set ".mounts=[${_mounts_json}]" \
      --set '.containerd.system=false' \
      --set '.containerd.user=false' \
      "${_clone_memory_args[@]}" \
      --tty=false &>/dev/null

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

    local _has_balloon=false
    if command -v qemu-system-x86_64 &>/dev/null; then
      [ -n "$max_memory" ] && limactl edit "$vm_name" --memory="$max_memory" --tty=false &>/dev/null
      if [ ${#_usb_vidpids[@]} -gt 0 ]; then
        _usb_qemu_wrapper="$(_agent_vm_qemu_wrapper "${_usb_vidpids[@]}")"
      else
        _usb_qemu_wrapper="$(_agent_vm_qemu_wrapper)"
      fi
      if QEMU_SYSTEM_X86_64="$_usb_qemu_wrapper" limactl start "$vm_name" &>/dev/null; then
        _has_balloon=true
      else
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
      if $_has_balloon; then
        local _balloon_daemon_args=()
        [ -n "$max_memory" ] && _balloon_daemon_args+=(--max-memory "${max_memory}G")
        python3 "$SCRIPT_DIR/balloon-daemon.py" "$HOME/.lima/$vm_name/qmp.sock" daemon "${_balloon_daemon_args[@]}" &>/dev/null &
        _balloon_daemon_pid=$!
      fi
    else
      [ -n "$max_memory" ] && echo "Warning: --max-memory ignored without balloon (QEMU not available)" >&2
      [ -n "$memory" ] && limactl edit "$vm_name" --memory="$memory" --tty=false &>/dev/null
      limactl start "$vm_name" &>/dev/null
    fi
  fi

  _claude_vm_post_boot_setup

  # ── Update agents and open shell (platform-specific) ─────────────────────
  if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
    echo "Updating agents..."
    wsl.exe -d "$vm_name" -u user -- bash -lc 'claude update --yes 2>/dev/null || true'
    wsl.exe -d "$vm_name" -u user -- bash -lc 'opencode update 2>/dev/null || true'
    wsl.exe -d "$vm_name" -u user -- bash -lc \
      '(curl -fsSL https://github.com/openai/codex/releases/latest/download/install.sh | sh) >/dev/null 2>&1 || true'
    wsl.exe -d "$vm_name" -u user -- bash -lc \
      '(curl -fsSL https://gh.io/copilot-install | sed "s|/dev/tty|/dev/stdin|g" | bash < /dev/null) >/dev/null 2>&1 || true'

    echo "WSL2 distro: $vm_name | Dir: $host_dir${agent:+ | Agent: $agent}"
    _claude_vm_print_config
    echo "OpenCode config: ${state_dir}/opencode-config/opencode.json"
    echo "Type 'exit' to stop and delete the WSL2 distro"

    CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
      wsl.exe -d "$vm_name" -u user -- \
      env ENABLE_LSP_TOOL=1 IS_SANDBOX=1 \
      OPENCODE_CONFIG="${state_dir}/opencode-config/opencode.json" \
      bash -lc "cd '$host_dir' && bash -l"
  else
    echo "Updating Claude Code..."
    limactl shell "$vm_name" bash -lc 'claude update --yes 2>/dev/null || true'
    echo "Updating OpenCode..."
    limactl shell "$vm_name" bash -lc 'opencode update 2>/dev/null || true'
    echo "Updating Codex..."
    limactl shell "$vm_name" bash -lc '(curl -fsSL https://github.com/openai/codex/releases/latest/download/install.sh | sh) >/dev/null 2>&1 || true'
    echo "Updating Copilot CLI..."
    limactl shell "$vm_name" bash -lc '(curl -fsSL https://gh.io/copilot-install | sed "s|/dev/tty|/dev/stdin|g" | bash < /dev/null) >/dev/null 2>&1 || true'

    echo "VM: $vm_name | Dir: $host_dir${agent:+ | Agent: $agent}"
    _claude_vm_print_config
    echo "OpenCode config: ${state_dir}/opencode-config/opencode.json"
    echo "Type 'exit' to stop and delete the VM"
    local shell_env=(
      ENABLE_LSP_TOOL=1
      IS_SANDBOX=1
      OPENCODE_CONFIG="${state_dir}/opencode-config/opencode.json"
    )
    [ -n "$_openai_token" ] && shell_env+=(OPENAI_API_KEY=dummy-key-auth-handled-by-proxy)
    CLIPBOARD_DIR="$state_dir" python3 "$SCRIPT_DIR/clipboard-pty.py" \
      limactl shell --workdir "$host_dir" "$vm_name" \
      env "${shell_env[@]}" \
      bash -l
  fi

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
    if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
      echo "Run AI coding agents inside sandboxed WSL2 distros (Windows)."
    else
      echo "Run AI coding agents inside sandboxed Lima VMs (macOS/Linux)."
    fi
    echo ""
    echo "Commands:"
    echo "  setup              Create the VM/distro template (run once)"
    echo "  claude [args]      Run Claude Code in a sandboxed VM/distro"
    echo "  opencode [args]    Run OpenCode in a sandboxed VM/distro"
    echo "  codex [args]       Run Codex in a sandboxed VM/distro"
    echo "  copilot [args]     Run GitHub Copilot CLI in a sandboxed VM/distro"
    echo "  shell [agent]      Open a debug shell (optionally pre-configured for an agent)"
    if [ "$_AGENT_VM_BACKEND" != "wsl2" ]; then
      echo "  memory [vm] [size] Query or adjust VM memory via balloon (e.g. 'memory 12G')"
    fi
    echo ""
    echo "Options (for claude, opencode, codex, copilot, shell):"
    echo "  --no-git           Skip GitHub integration"
    echo "  --mount DIR        Mount additional directory into the VM/distro"
    if [ "$_AGENT_VM_BACKEND" != "wsl2" ]; then
      echo "  --memory GB        Initial memory for the VM (default: 2G with balloon, 4G without)"
      echo "  --max-memory GB    Memory ceiling (default: from template; balloon grows up to this)"
      echo "  --usb DEVICE       Pass USB device to VM (repeatable; /dev/ttyACM0 or 1a86:55d3)"
      echo ""
      echo "Memory management:"
      echo "  On Linux (QEMU), VMs use a virtio-balloon to start with 2G and auto-grow"
      echo "  up to the ceiling as the guest needs more memory."
      echo "  Example: agent-vm claude --memory 5 --max-memory 10"
      echo "  On macOS (VZ), --memory sets a fixed allocation (default: 4G, no balloon)."
    fi
    echo ""
    echo "Run 'agent-vm <command> --help' for command-specific help."
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
      if [ "$_AGENT_VM_BACKEND" = "wsl2" ]; then
        echo "Error: 'agent-vm memory' (balloon control) is not supported on WSL2." >&2
        return 1
      fi
      _agent_vm_memory "$@"
      ;;
    *)
      echo "Error: Unknown command '$subcmd'" >&2
      echo "Run 'agent-vm' for usage." >&2
      return 1
      ;;
  esac
}
