# agent-vm

Run AI coding agents inside sandboxed Linux VMs. The agent gets full autonomy while your host system stays safe.

Uses [Lima](https://lima-vm.io/) to create lightweight Debian VMs on macOS and Linux. Ships with dev tools, Docker, and a headless Chrome browser with [Chrome DevTools MCP](https://github.com/ChromeDevTools/chrome-devtools-mcp) pre-configured.

Supports [Claude Code](https://claude.ai/code) and [OpenCode](https://opencode.ai/). Other agents (Codex, etc.) can be added in the future.

Feedbacks welcome!

## Prerequisites

- macOS or Linux
- [Lima](https://lima-vm.io/docs/installation/) (installed automatically via Homebrew if available)
- A [Claude subscription](https://claude.ai/) (Pro, Max, or Team) and/or an [OpenCode](https://opencode.ai/) compatible provider

## Install

```bash
git clone https://github.com/sylvinus/agent-vm.git
cd agent-vm

# Add to your shell config
echo "source $(pwd)/claude-vm.sh" >> ~/.zshrc   # zsh
echo "source $(pwd)/claude-vm.sh" >> ~/.bashrc  # or bash
```

## Usage

### One-time setup

```bash
agent-vm setup
```

Creates a VM template with dev tools, Docker, Chromium, Claude Code, and OpenCode pre-installed. During setup, Claude will launch once for authentication. After it responds, type `/exit` to continue with the rest of the setup. (We haven't found a way to automate this step yet.)

Options:

| Flag | Description | Default |
|------|-------------|---------|
| `--minimal` | Only install git, curl, jq, Claude Code, and OpenCode. Skips Docker, Node.js, Python, Chromium, and the Chrome MCP server. | off |
| `--disk GB` | VM disk size in GB | 20 |
| `--memory GB` | VM memory in GB | 8 |

```bash
agent-vm setup --minimal                  # Lightweight VM with only core CLIs
agent-vm setup --disk 50 --memory 16      # Larger VM for heavy workloads
```

### Run Claude in a VM

```bash
cd your-project
agent-vm claude
```

Clones the template into a fresh VM, mounts your current directory, and runs `claude --dangerously-skip-permissions` with `IS_SANDBOX=1` to suppress the dangerous mode confirmation prompt (the VM itself is the sandbox). The VM is deleted when Claude exits.

Any arguments are forwarded to the `claude` command:

```bash
agent-vm claude -p "fix all lint errors"        # Run with a prompt
agent-vm claude --resume                         # Resume previous session
agent-vm claude -c "explain this codebase"       # Continue conversation
```

### Run OpenCode in a VM

```bash
cd your-project
agent-vm opencode
```

Runs [OpenCode](https://opencode.ai/) inside a sandboxed VM instead of Claude Code. Uses the same VM template, proxies, and security model. OpenCode is configured to use the Anthropic provider through the host proxy with all permissions auto-approved (the VM is the sandbox).

Any arguments are forwarded to the `opencode` command:

```bash
agent-vm opencode run "fix all lint errors"        # Non-interactive mode
agent-vm opencode --continue                        # Continue last session
agent-vm opencode -m anthropic/claude-sonnet-4-5    # Specify a model
```

### Debug shell

```bash
agent-vm shell                # Plain shell with API proxy configured
agent-vm shell claude         # Shell pre-configured for Claude
agent-vm shell opencode       # Shell pre-configured for OpenCode
```

Drops you into a bash shell inside a fresh VM instead of launching an agent. Useful for debugging or manual testing. When an agent is specified, the shell has that agent's configuration (env vars, MCP servers, etc.) pre-applied.

## Customization

### Per-user: `~/.claude-vm.setup.sh`

Create this file in your home directory to install extra tools into the VM template. It runs once during `agent-vm setup`, as the default VM user (with sudo available):

```bash
# ~/.claude-vm.setup.sh
sudo apt-get install -y postgresql-client
pip install pandas numpy
```

### Per-project: `.claude-vm.runtime.sh`

Create this file at the root of any project. It runs inside the cloned VM each time you run an agent, just before the agent starts. Use it for project-specific setup like installing dependencies or starting services:

```bash
# your-project/.claude-vm.runtime.sh
npm install
docker compose up -d
```

## Session persistence

VMs are ephemeral — each invocation creates a fresh clone and deletes it on exit. To make session resume work across VM launches, agent state is persisted outside your project directory.

**How it works:**

1. A per-project state directory is created under `${XDG_STATE_HOME:-~/.local/state}/agent-vm/`
2. Claude's session-related directories (`projects`, `file-history`, `todos`, `plans`) and `history.jsonl` are symlinked from `~/.claude/` to `<state-dir>/claude-sessions/`
3. Ephemeral config (`.credentials.json`, `CLAUDE.md`) stays in-VM and is not persisted
4. `~/.claude.json` is persisted as `<state-dir>/claude-sessions/claude.json` to preserve onboarding/project state
5. `hasCompletedOnboarding=true` is enforced before launch to prevent first-run greeting loops

Set `AGENT_VM_STATE_DIR` to override the state root location.

```bash
agent-vm claude -p "remember ZEBRA"        # First session
agent-vm claude --continue                  # Picks up where the last session left off
```

`agent-vm claude` now launches Claude directly without a hidden priming request.

### OpenCode sessions and configuration

OpenCode session data is persisted in `<state-dir>/opencode-sessions/` by symlinking `~/.local/share/opencode/` inside the VM.

OpenCode configuration is stored in `<state-dir>/opencode-config/opencode.json` and referenced via the `OPENCODE_CONFIG` env var. It configures:
- The Anthropic provider pointing to the host proxy
- All permissions set to `"allow"` (the VM is the sandbox)
- GitHub MCP server (when available)
- Autoupdates disabled

## Claude API Proxy

The host-side API proxy (`claude-vm-proxy.py`) keeps your Claude credentials out of the VM entirely. It reads OAuth tokens from the host's `~/.claude/.credentials.json` (or `ANTHROPIC_API_KEY` env var), injects them into requests, and forwards to `api.anthropic.com`. The VM only sees `ANTHROPIC_BASE_URL` pointing at the proxy.

**OAuth token refresh:** When the OAuth access token is about to expire (within 5 minutes), the proxy automatically refreshes it using the standard OAuth `refresh_token` grant against `platform.claude.com`. The refreshed token is saved back to disk atomically (with file locking for concurrency safety). This uses only Python stdlib (`http.client`) — no external dependencies required.

| Env var | Description | Default |
|---------|-------------|---------|
| `ANTHROPIC_API_KEY` | API key (takes priority over OAuth) | — |
| `CLAUDE_VM_PROXY_DEBUG` | Set to `1` for verbose logging | `0` |
| `CLAUDE_VM_PROXY_LOG_DIR` | Directory for log file | `.` |

## GitHub Integration

When you run `agent-vm claude` or `agent-vm opencode` inside a git repo with a GitHub remote, it automatically:

1. Detects the repository from `git remote`
2. Obtains a repo-scoped GitHub token via the device flow (browser-based OAuth)
3. Starts two host-side proxies: one for the GitHub MCP Server, one for Git HTTP
4. Configures the VM so both `git push`/`pull` and MCP tools work transparently
5. Writes instructions to `~/.claude/CLAUDE.md` in the VM so the agent knows git is available

No credentials are ever exposed to the VM. Both proxies inject the token on the host side and enforce repo scope.

### Token generation and scoping

Tokens are generated via a [GitHub App](https://docs.github.com/en/apps) using the [device flow](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#device-flow):

1. **One-time setup**: Create a GitHub App with `contents: write` permission and install it on your org/account. The App's Client ID is configured in `claude-vm.sh`.
2. **Per-session**: `github_app_token_demo.py` initiates the device flow — you approve in a browser, and a user access token is returned.
3. **Repo scoping**: The `--repo` flag resolves the repository's numeric ID and passes it as `repository_id` during the OAuth token exchange. GitHub scopes the resulting token to that single repository at the API level.
4. **Caching**: Tokens are cached in `~/.cache/claude-vm/` and automatically refreshed when expired, so you only need to re-authorize when the refresh token expires.

### Git HTTP Proxy

The Git HTTP proxy (`github-git-proxy.py`) lets the VM push and pull via standard git commands without SSH keys or tokens in the VM.

**How it works:**

1. The proxy runs on the host, listening on HTTP
2. `claude-vm.sh` configures git's `url.<proxy>.insteadOf` in the VM's `~/.gitconfig` to rewrite the repo's SSH and HTTPS URLs through the proxy
3. The proxy injects Basic auth (`x-access-token:TOKEN`) for requests matching the configured repo
4. Requests for other repos are forwarded without credentials (they fail auth on GitHub's side)
5. The host's git `user.name` and `user.email` are copied into the VM

**Configuration:**

| Env var | Description | Default |
|---------|-------------|---------|
| `GITHUB_MCP_TOKEN` | GitHub token (required) | — |
| `GITHUB_MCP_OWNER` | Repository owner (required) | — |
| `GITHUB_MCP_REPO` | Repository name (required) | — |
| `GITHUB_GIT_PROXY_DEBUG` | Set to `1` for verbose logging | `0` |
| `GITHUB_GIT_PROXY_LOG_DIR` | Directory for log file | `.` |

### GitHub MCP Proxy

The GitHub MCP proxy (`github-mcp-proxy.py`) gives the VM access to GitHub's [MCP Server](https://github.com/github/github-mcp-server) for issues, PRs, code search, and other API operations.

**Defense-in-depth:**

Even though the token is already scoped to one repository by GitHub, the proxy adds multiple enforcement layers:

| Layer | Mechanism |
|-------|-----------|
| **Owner/repo check** | Tool arguments with `owner`/`repo` must match the configured repo. Missing values are auto-injected. |
| **Search query scoping** | `repo:OWNER/REPO` is injected into search queries. `org:` and `user:` qualifiers are rejected. |
| **Tool allowlist** | Unknown tools are blocked by default (default-deny). Non-repo-scoped tools (`search_users`, `get_teams`, etc.) are blocked. |
| **Server-side filtering** | `X-MCP-Toolsets` header limits GitHub's server to `repos,issues,pull_requests,git,labels` by default. |
| **Lockdown mode** | `X-MCP-Lockdown` is enabled by default, hiding issue details from users without push access. |
| **Header protection** | VM cannot override `X-MCP-*` headers — the proxy strips them before injecting host-configured values. |

**Configuration:**

| Env var | Description | Default |
|---------|-------------|---------|
| `GITHUB_MCP_TOKEN` | GitHub token (required) | — |
| `GITHUB_MCP_OWNER` | Repository owner (required) | — |
| `GITHUB_MCP_REPO` | Repository name (required) | — |
| `GITHUB_MCP_TOOLSETS` | Comma-separated [toolsets](https://github.com/github/github-mcp-server/blob/main/docs/remote-server.md) | `repos,issues,pull_requests,git,labels` |
| `GITHUB_MCP_TOOLS` | Comma-separated tool names (fine-grained) | *(all in allowed toolsets)* |
| `GITHUB_MCP_READONLY` | Set to `1` for read-only mode | `0` |
| `GITHUB_MCP_LOCKDOWN` | Set to `0` to disable lockdown | `1` |
| `GITHUB_MCP_PROXY_DEBUG` | Set to `1` for verbose logging | `0` |

### Standalone usage

Both proxies can be run independently of agent-vm:

```bash
# MCP proxy
GITHUB_MCP_TOKEN=ghu_... GITHUB_MCP_OWNER=myorg GITHUB_MCP_REPO=myrepo \
  python3 github-mcp-proxy.py

# Git HTTP proxy
GITHUB_MCP_TOKEN=ghu_... GITHUB_MCP_OWNER=myorg GITHUB_MCP_REPO=myrepo \
  python3 github-git-proxy.py
# Both print the listening port to stdout
```

## How it works

1. **`agent-vm setup`** creates a Debian 13 VM with Lima, installs dev tools + Chrome + Claude Code + OpenCode, and stops it as a reusable template
2. **`agent-vm claude [args]`** clones the template, mounts your working directory read-write, runs optional `.claude-vm.runtime.sh`, then launches Claude with full permissions (forwarding any arguments to the `claude` command)
3. **`agent-vm opencode [args]`** same as above but launches OpenCode instead, with its own config and session persistence
4. **`agent-vm shell [agent]`** same VM setup but drops into a bash shell for debugging
5. On exit, the cloned VM is stopped and deleted. The template persists for reuse

Ports opened inside the VM (e.g. by Docker containers) are automatically forwarded to your host by Lima.

## What's in the VM

| Category | Packages |
|----------|----------|
| Core | git, curl, wget, build-essential, jq |
| Python | python3, pip, venv |
| Node.js | Node.js 22 (via NodeSource) |
| Search | ripgrep, fd-find |
| Browser | Chromium (headless), xvfb |
| Containers | Docker Engine, Docker Compose |
| AI | Claude Code, OpenCode, Chrome DevTools MCP server |

## Why a VM?

Running an AI agent with full permissions is powerful but risky. Here's how the options compare:

| | No sandbox | Docker | VM (agent-vm) |
|---|---|---|---|
| Agent can run any command | Yes | Yes | Yes |
| File system isolation | None | Partial (shared kernel) | Full |
| Network isolation | None | Partial | Full |
| Can run Docker inside | Yes | Requires DinD or socket mount | Yes (native) |
| Kernel-level isolation | None | None (shares host kernel) | Full (separate kernel) |
| Protection from container escapes | None | None | Yes |
| Browser / GUI tools | Host only | Complex setup | Built-in (headless Chromium) |

Docker containers share the host kernel, so a motivated agent could exploit kernel vulnerabilities or misconfigurations to escape. A VM runs its own kernel — even if the agent gains root inside the VM, it can't reach the host.

A VM also avoids the practical headaches of Docker sandboxing. Docker runs natively inside the VM without Docker-in-Docker hacks or socket mounts. Headless Chromium works out of the box without fiddling with `--no-sandbox` flags or shared memory settings. Lima automatically forwards ports from the VM to your host, so if the agent starts a server on port 3000, it's immediately accessible at `localhost:3000`. The agent gets a normal Linux environment where everything just works.

Finally, using a VM means you don't need Node.js, npm, Docker, or any other dev tooling installed on your host machine. The only host dependency is Lima. All the tools (and their vulnerabilities) live inside the VM.

For AI agents running with `--dangerously-skip-permissions`, a VM is the only sandbox that provides meaningful security.

## License

MIT
