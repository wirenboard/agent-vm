#!/usr/bin/env bash
#
# agent-vm / claude-vm: Run Claude Code inside a sandboxed Lima VM
# Part of https://github.com/sylvinus/agent-vm
#
# Source this file in your shell config:
#   source /path/to/agent-vm/claude-vm.sh
#
# Functions:
#   claude-vm-setup  - Create the VM template (run once)
#   claude-vm [args] - Run Claude in a fresh VM with cwd mounted (args forwarded to claude)
#   claude-vm-shell  - Open a debug shell in a fresh VM

CLAUDE_VM_TEMPLATE="claude-template"

claude-vm-setup() {
  local minimal=false
  local disk=20
  local memory=8

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --help|-h)
        echo "Usage: claude-vm-setup [--minimal] [--disk GB] [--memory GB]"
        echo ""
        echo "Create a VM template with Claude Code pre-installed."
        echo ""
        echo "Options:"
        echo "  --minimal      Only install git, curl, jq, and Claude Code"
        echo "  --disk GB      VM disk size (default: 20)"
        echo "  --memory GB    VM memory (default: 8)"
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
        echo "Usage: claude-vm-setup [--minimal] [--disk GB] [--memory GB]" >&2
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
    --disk="$disk" \
    --memory="$memory" \
    --tty=false
  limactl start "$CLAUDE_VM_TEMPLATE"

  # Disable needrestart's interactive prompts
  limactl shell "$CLAUDE_VM_TEMPLATE" sudo bash -c 'mkdir -p /etc/needrestart/conf.d && echo "\$nrconf{restart} = '"'"'a'"'"';" > /etc/needrestart/conf.d/no-prompt.conf'

  echo "Installing base packages..."
  limactl shell "$CLAUDE_VM_TEMPLATE" sudo DEBIAN_FRONTEND=noninteractive apt-get update
  limactl shell "$CLAUDE_VM_TEMPLATE" sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
    git curl jq

  if ! $minimal; then
    limactl shell "$CLAUDE_VM_TEMPLATE" sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
      wget build-essential \
      python3 python3-pip python3-venv \
      ripgrep fd-find htop \
      unzip zip \
      ca-certificates

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
  fi

  # Install Claude Code
  echo "Installing Claude Code..."
  limactl shell "$CLAUDE_VM_TEMPLATE" bash -c "curl -fsSL https://claude.ai/install.sh | bash"
  limactl shell "$CLAUDE_VM_TEMPLATE" bash -c 'echo "export PATH=\$HOME/.local/bin:\$HOME/.claude/local/bin:\$PATH" >> ~/.bashrc'

  # Authenticate Claude (saves token in template, inherited by clones)
  echo "Setting up Claude authentication..."
  limactl shell "$CLAUDE_VM_TEMPLATE" bash -lc "claude 'Ok I am logged in, I can exit now.'"

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

  # Run user's custom setup script if it exists
  local user_setup="$HOME/.claude-vm.setup.sh"
  if [ -f "$user_setup" ]; then
    echo "Running custom setup from $user_setup..."
    limactl shell "$CLAUDE_VM_TEMPLATE" bash < "$user_setup"
  fi

  limactl stop "$CLAUDE_VM_TEMPLATE"

  echo "Template ready. Run 'claude-vm' in any project directory."
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

claude-vm() {
  local args=("$@")
  local vm_name="claude-$(basename "$(pwd)" | tr -cs 'a-zA-Z0-9' '-' | sed 's/^-//;s/-$//')-$$"
  local host_dir="$(pwd)"

  if ! limactl list -q 2>/dev/null | grep -q "^${CLAUDE_VM_TEMPLATE}$"; then
    echo "Error: Template VM not found. Run 'claude-vm-setup' first." >&2
    return 1
  fi

  # Security: snapshot before session
  local security_snapshot
  security_snapshot="$(mktemp)"

  _claude_vm_cleanup() {
    echo "Cleaning up VM..."
    limactl stop "$vm_name" &>/dev/null
    limactl delete "$vm_name" --force &>/dev/null
    rm -f "$security_snapshot" "${security_snapshot}.git-config"
  }
  trap _claude_vm_cleanup EXIT INT TERM

  _claude_vm_security_snapshot "$host_dir" "$security_snapshot"

  # Ensure .git/hooks exists for read-only mount
  mkdir -p "${host_dir}/.git/hooks"

  echo "Starting VM '$vm_name'..."
  # Mount project dir writable, but .git/hooks and .git/config read-only at the Lima level (VM root cannot override)
  limactl clone "$CLAUDE_VM_TEMPLATE" "$vm_name" \
    --set ".mounts=[{\"location\":\"${host_dir}\",\"writable\":true},{\"location\":\"${host_dir}/.git/hooks\",\"writable\":false},{\"location\":\"${host_dir}/.git/config\",\"writable\":false}]" \
    --tty=false &>/dev/null

  limactl start "$vm_name" &>/dev/null

  # Run project-specific runtime script if it exists
  if [ -f "${host_dir}/.claude-vm.runtime.sh" ]; then
    echo "Running project runtime setup..."
    limactl shell --workdir "$host_dir" "$vm_name" bash -l < "${host_dir}/.claude-vm.runtime.sh"
  fi

  limactl shell --workdir "$host_dir" "$vm_name" claude --dangerously-skip-permissions "${args[@]}"
  _claude_vm_security_check "$host_dir" "$security_snapshot"

  _claude_vm_cleanup
  trap - EXIT INT TERM
}

claude-vm-shell() {
  local vm_name="claude-debug-$$"
  local host_dir="$(pwd)"

  if ! limactl list -q 2>/dev/null | grep -q "^${CLAUDE_VM_TEMPLATE}$"; then
    echo "Error: Template VM not found. Run 'claude-vm-setup' first." >&2
    return 1
  fi

  # Security: snapshot before session
  local security_snapshot
  security_snapshot="$(mktemp)"

  _claude_vm_shell_cleanup() {
    echo "Cleaning up VM..."
    limactl stop "$vm_name" &>/dev/null
    limactl delete "$vm_name" --force &>/dev/null
    rm -f "$security_snapshot" "${security_snapshot}.git-config"
  }
  trap _claude_vm_shell_cleanup EXIT INT TERM

  _claude_vm_security_snapshot "$host_dir" "$security_snapshot"

  # Ensure .git/hooks exists for read-only mount
  mkdir -p "${host_dir}/.git/hooks"

  # Mount project dir writable, but .git/hooks and .git/config read-only at the Lima level (VM root cannot override)
  limactl clone "$CLAUDE_VM_TEMPLATE" "$vm_name" \
    --set ".mounts=[{\"location\":\"${host_dir}\",\"writable\":true},{\"location\":\"${host_dir}/.git/hooks\",\"writable\":false},{\"location\":\"${host_dir}/.git/config\",\"writable\":false}]" \
    --tty=false &>/dev/null

  limactl start "$vm_name" &>/dev/null

  # Run project-specific runtime script if it exists
  if [ -f "${host_dir}/.claude-vm.runtime.sh" ]; then
    limactl shell --workdir "$host_dir" "$vm_name" bash -l < "${host_dir}/.claude-vm.runtime.sh"
  fi

  echo "VM: $vm_name | Dir: $host_dir"
  echo "Type 'exit' to stop and delete the VM"
  limactl shell --workdir "$host_dir" "$vm_name" bash -l
  _claude_vm_security_check "$host_dir" "$security_snapshot"

  _claude_vm_shell_cleanup
  trap - EXIT INT TERM
}
