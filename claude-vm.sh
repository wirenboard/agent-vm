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

_agent_vm_setup() {
  local minimal=false
  local disk=20
  local memory=8

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --help|-h)
        echo "Usage: agent-vm setup [--minimal] [--disk GB] [--memory GB]"
        echo ""
        echo "Create a VM template with Claude Code and OpenCode pre-installed."
        echo ""
        echo "Options:"
        echo "  --minimal      Only install git, curl, jq, Claude Code, and OpenCode"
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

  # Skip first-run onboarding wizard (theme picker + login prompt)
  limactl shell "$CLAUDE_VM_TEMPLATE" bash -c '
    mkdir -p ~/.claude
    echo "{\"theme\":\"dark\",\"hasCompletedOnboarding\":true,\"skipDangerousModePermissionPrompt\":true,\"effortLevel\":\"high\"}" > ~/.claude/settings.json
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

_claude_vm_start_github_mcp() {
  local host_dir="$1"
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

  # Detect repo from git remote
  local repo_url
  repo_url=$(git -C "$host_dir" remote get-url origin 2>/dev/null)
  if [ -z "$repo_url" ]; then
    echo "Warning: No git remote found, skipping GitHub MCP" >&2
    return 1
  fi

  # Parse owner/repo from remote URL
  local owner repo
  read -r owner repo < <(python3 -c "
import re, sys
url = sys.argv[1]
for pat in [r'git@github\.com:([^/]+)/([^/]+?)(?:\.git)?$',
            r'https?://github\.com/([^/]+)/([^/]+?)(?:\.git)?(?:/.*)?$']:
    m = re.match(pat, url)
    if m:
        print(m.group(1), m.group(2))
        sys.exit(0)
sys.exit(1)
" "$repo_url")
  if [ -z "$owner" ] || [ -z "$repo" ]; then
    echo "Warning: Cannot parse GitHub remote '$repo_url', skipping GitHub MCP" >&2
    return 1
  fi

  # Get scoped user token via device flow
  echo "Requesting GitHub token for $owner/$repo..."
  local token
  token=$(python3 "$script_dir/github_app_token_demo.py" \
    user-token --client-id Iv23liisR1WdpJmDUPLT \
    --repo "$repo_url" --token-only \
    --cache-dir "$HOME/.cache/claude-vm")
  if [ -z "$token" ]; then
    echo "Warning: Failed to get GitHub token, skipping GitHub MCP" >&2
    return 1
  fi

  # Start GitHub MCP proxy (injects token, enforces repo scope)
  echo "Starting GitHub MCP proxy..."
  exec 4< <(GITHUB_MCP_TOKEN="$token" GITHUB_MCP_OWNER="$owner" GITHUB_MCP_REPO="$repo" \
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
  echo "GitHub MCP proxy on port $_claude_vm_github_mcp_port (scope: $owner/$repo)"

  # Start Git HTTP proxy (injects token, enforces repo scope)
  echo "Starting Git HTTP proxy..."
  exec 5< <(GITHUB_MCP_TOKEN="$token" GITHUB_MCP_OWNER="$owner" GITHUB_MCP_REPO="$repo" \
    python3 "$script_dir/github-git-proxy.py")
  _claude_vm_git_proxy_pid=$!
  if ! read -r -t 5 _claude_vm_git_proxy_port <&5; then
    echo "Warning: Git HTTP proxy failed to start" >&2
    kill "$_claude_vm_git_proxy_pid" 2>/dev/null
    exec 5<&-
    # Non-fatal: MCP still works, just no git push
  else
    echo "Git HTTP proxy on port $_claude_vm_git_proxy_port (scope: $owner/$repo)"
  fi
  exec 5<&-

  # Export for use by other functions
  _claude_vm_github_owner="$owner"
  _claude_vm_github_repo="$repo"
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
  local owner="$3"
  local repo="$4"
  local proxy_base="http://host.lima.internal:${git_port}/${owner}/${repo}"

  # Get git user identity from host
  local git_name git_email
  git_name=$(git config user.name 2>/dev/null || echo "")
  git_email=$(git config user.email 2>/dev/null || echo "")

  # Rewrite only this repo's URLs through the proxy.
  # git insteadOf is prefix-matched, so "git@github.com:owner/repo" matches
  # both "git@github.com:owner/repo.git" and "git@github.com:owner/repo".
  limactl shell "$vm_name" bash -c "
    git config --global url.\"${proxy_base}\".insteadOf \"git@github.com:${owner}/${repo}\"
    git config --global --add url.\"${proxy_base}\".insteadOf \"https://github.com/${owner}/${repo}\"
    ${git_name:+git config --global user.name \"$git_name\"}
    ${git_email:+git config --global user.email \"$git_email\"}
  "

  # Write Claude instructions about git access
  limactl shell "$vm_name" bash -c "
    mkdir -p \$HOME/.claude
    cat >> \$HOME/.claude/CLAUDE.md << 'INSTRUCTIONS'

# Git access

Git push and pull to GitHub work out of the box for this repository.
The remote URL is automatically rewritten to go through a host-side proxy
that injects credentials. You can use standard git commands:

    git push origin main
    git push origin HEAD:my-branch
    git pull origin main

Only ${owner}/${repo} has credentials configured. Other repos will require
their own authentication.
INSTRUCTIONS
  "
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
  local owner="$3"
  local repo="$4"
  local host_dir="$5"
  local proxy_base="http://host.lima.internal:${git_port}/${owner}/${repo}"

  # Get git user identity from host
  local git_name git_email
  git_name=$(git config user.name 2>/dev/null || echo "")
  git_email=$(git config user.email 2>/dev/null || echo "")

  # Rewrite only this repo's URLs through the proxy
  # (Claude's _claude_vm_inject_git_proxy already does this, but we call it
  #  for the opencode-only path where claude's version might not be called)
  limactl shell "$vm_name" bash -c "
    git config --global url.\"${proxy_base}\".insteadOf \"git@github.com:${owner}/${repo}\"
    git config --global --add url.\"${proxy_base}\".insteadOf \"https://github.com/${owner}/${repo}\"
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
  local args=()
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --no-git) use_github=false; shift ;;
      *) args+=("$1"); shift ;;
    esac
  done
  local vm_name="${agent}-$(basename "$(pwd)" | tr -cs 'a-zA-Z0-9' '-' | sed 's/^-//;s/-$//')-$$"
  local host_dir="$(pwd)"
  local state_dir
  state_dir="$(_agent_vm_project_state_dir "$host_dir")"

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
    [ -n "$_claude_vm_proxy_pid" ] && kill "$_claude_vm_proxy_pid" 2>/dev/null
    [ -n "$_claude_vm_github_mcp_pid" ] && kill "$_claude_vm_github_mcp_pid" 2>/dev/null
    [ -n "$_claude_vm_git_proxy_pid" ] && kill "$_claude_vm_git_proxy_pid" 2>/dev/null
    limactl stop "$vm_name" &>/dev/null
    limactl delete "$vm_name" --force &>/dev/null
    rm -f "$security_snapshot" "${security_snapshot}.git-config"
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

  limactl start "$vm_name" &>/dev/null

  _claude_vm_write_dummy_credentials "$vm_name"
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
  if $use_github && [ -n "$_claude_vm_git_proxy_port" ]; then
    _claude_vm_inject_git_proxy "$vm_name" "$_claude_vm_git_proxy_port" \
      "$_claude_vm_github_owner" "$_claude_vm_github_repo"
    if [ "$agent" = "opencode" ]; then
      _opencode_vm_inject_git_proxy "$vm_name" "$_claude_vm_git_proxy_port" \
        "$_claude_vm_github_owner" "$_claude_vm_github_repo" "$host_dir"
    fi
  fi

  # Run project-specific runtime script if it exists
  if [ -f "${host_dir}/.claude-vm.runtime.sh" ]; then
    echo "Running project runtime setup..."
    limactl shell --workdir "$host_dir" "$vm_name" bash -l < "${host_dir}/.claude-vm.runtime.sh"
  fi

  if [ "$agent" = "opencode" ]; then
    limactl shell --workdir "$host_dir" "$vm_name" \
      env OPENCODE_CONFIG="${state_dir}/opencode-config/opencode.json" \
      opencode "${args[@]}"
  else
    local claude_args=("${args[@]}")
    limactl shell --workdir "$host_dir" "$vm_name" \
      env ANTHROPIC_BASE_URL="http://host.lima.internal:${_claude_vm_proxy_port}" \
      IS_SANDBOX=1 \
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

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --no-git) use_github=false; shift ;;
      *) shift ;;
    esac
  done
  local vm_name="${agent:+${agent}-}debug-$$"
  local host_dir="$(pwd)"
  local state_dir
  state_dir="$(_agent_vm_project_state_dir "$host_dir")"

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
    [ -n "$_claude_vm_proxy_pid" ] && kill "$_claude_vm_proxy_pid" 2>/dev/null
    [ -n "$_claude_vm_github_mcp_pid" ] && kill "$_claude_vm_github_mcp_pid" 2>/dev/null
    [ -n "$_claude_vm_git_proxy_pid" ] && kill "$_claude_vm_git_proxy_pid" 2>/dev/null
    limactl stop "$vm_name" &>/dev/null
    limactl delete "$vm_name" --force &>/dev/null
    rm -f "$security_snapshot" "${security_snapshot}.git-config"
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

  limactl start "$vm_name" &>/dev/null

  _claude_vm_write_dummy_credentials "$vm_name"
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
  if $use_github && [ -n "$_claude_vm_git_proxy_port" ]; then
    _claude_vm_inject_git_proxy "$vm_name" "$_claude_vm_git_proxy_port" \
      "$_claude_vm_github_owner" "$_claude_vm_github_repo"
    if [ "$agent" = "opencode" ]; then
      _opencode_vm_inject_git_proxy "$vm_name" "$_claude_vm_git_proxy_port" \
        "$_claude_vm_github_owner" "$_claude_vm_github_repo" "$host_dir"
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
  local shell_env=(ANTHROPIC_BASE_URL="http://host.lima.internal:${_claude_vm_proxy_port}")
  if [ "$agent" = "opencode" ]; then
    shell_env+=(OPENCODE_CONFIG="${state_dir}/opencode-config/opencode.json")
  fi
  limactl shell --workdir "$host_dir" "$vm_name" \
    env "${shell_env[@]}" \
    bash -l
  _claude_vm_security_check "$host_dir" "$security_snapshot"

  _claude_vm_shell_cleanup
  trap - EXIT INT TERM
}

# ── Main entry point ──────────────────────────────────────────────────────────

agent-vm() {
  local subcmd="${1:-}"
  if [ -z "$subcmd" ]; then
    echo "Usage: agent-vm <command> [options]" >&2
    echo "" >&2
    echo "Commands:" >&2
    echo "  setup              Create the VM template (run once)" >&2
    echo "  claude [args]      Run Claude Code in a sandboxed VM" >&2
    echo "  opencode [args]    Run OpenCode in a sandboxed VM" >&2
    echo "  shell [agent]      Open a debug shell (optionally pre-configured for an agent)" >&2
    echo "" >&2
    echo "Options:" >&2
    echo "  --no-git           Skip GitHub integration" >&2
    return 1
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
    *)
      echo "Error: Unknown command '$subcmd'" >&2
      echo "Run 'agent-vm' for usage." >&2
      return 1
      ;;
  esac
}
