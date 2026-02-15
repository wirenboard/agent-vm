# agent-vm

Run AI coding agents inside sandboxed Linux VMs. The agent gets full autonomy while your host system stays safe.

Uses [Lima](https://lima-vm.io/) to create lightweight Debian VMs on macOS and Linux. Ships with dev tools, Docker, and a headless Chrome browser with [Chrome DevTools MCP](https://github.com/ChromeDevTools/chrome-devtools-mcp) pre-configured.

Currently supports [Claude Code](https://claude.ai/code). Other agents (Codex, etc.) can be added in the future.

Feedbacks welcome!

## Prerequisites

- macOS or Linux
- [Lima](https://lima-vm.io/docs/installation/) (installed automatically via Homebrew if available)
- A [Claude subscription](https://claude.ai/) (Pro, Max, or Team)

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
claude-vm-setup
```

Creates a VM template with dev tools, Docker, Chromium, and Claude Code pre-installed. During setup, Claude will launch once for authentication. After it responds, type `/exit` to continue with the rest of the setup. (We haven't found a way to automate this step yet.)

Options:

| Flag | Description | Default |
|------|-------------|---------|
| `--minimal` | Only install git, curl, jq, and Claude Code. Skips Docker, Node.js, Python, Chromium, and the Chrome MCP server. | off |
| `--disk GB` | VM disk size in GB | 20 |
| `--memory GB` | VM memory in GB | 8 |

```bash
claude-vm-setup --minimal                  # Lightweight VM with just Claude
claude-vm-setup --disk 50 --memory 16      # Larger VM for heavy workloads
```

### Run Claude in a VM

```bash
cd your-project
claude-vm
```

Clones the template into a fresh VM, mounts your current directory, and runs `claude --dangerously-skip-permissions`. The VM is deleted when Claude exits.

Any arguments passed to `claude-vm` are forwarded to the `claude` command:

```bash
claude-vm -p "fix all lint errors"        # Run with a prompt
claude-vm --resume                         # Resume previous session
claude-vm -c "explain this codebase"       # Continue conversation
```

### Debug shell

```bash
claude-vm-shell
```

Same as `claude-vm` but drops you into a bash shell instead.

## Customization

### Per-user: `~/.claude-vm.setup.sh`

Create this file in your home directory to install extra tools into the VM template. It runs once during `claude-vm-setup`, as the default VM user (with sudo available):

```bash
# ~/.claude-vm.setup.sh
sudo apt-get install -y postgresql-client
pip install pandas numpy
```

### Per-project: `.claude-vm.runtime.sh`

Create this file at the root of any project. It runs inside the cloned VM each time you call `claude-vm`, just before Claude starts. Use it for project-specific setup like installing dependencies or starting services:

```bash
# your-project/.claude-vm.runtime.sh
npm install
docker compose up -d
```

## GitHub MCP Proxy

The GitHub MCP proxy (`github-mcp-proxy.py`) runs on the host and gives the VM access to a single GitHub repository via the [GitHub MCP Server](https://github.com/github/github-mcp-server), without exposing any credentials to the VM.

### How it works

1. The proxy runs on the host, listening on a local port
2. The VM connects to the proxy over Lima's host networking
3. The proxy injects the GitHub token and forwards requests to GitHub's MCP endpoint
4. **Repo scope enforcement** ensures the VM can only access the configured repository

### Security layers

The proxy enforces repo scoping through multiple layers:

| Layer | Mechanism |
|-------|-----------|
| **Owner/repo check** | Tool arguments with `owner`/`repo` must match the configured repo. Missing values are auto-injected. |
| **Search query scoping** | `repo:OWNER/REPO` is injected into search queries. `org:` and `user:` qualifiers are rejected. |
| **Tool allowlist** | Unknown tools are blocked by default (default-deny). Non-repo-scoped tools (`search_users`, `get_teams`, etc.) are blocked. |
| **Server-side filtering** | `X-MCP-Toolsets` header limits GitHub's server to `repos,issues,pull_requests,git,labels` by default. |
| **Lockdown mode** | `X-MCP-Lockdown` is enabled by default, hiding issue details from users without push access. |
| **Header protection** | VM cannot override `X-MCP-*` headers — the proxy strips them before injecting host-configured values. |

### Usage

```bash
GITHUB_MCP_TOKEN=ghu_... \
GITHUB_MCP_OWNER=myorg \
GITHUB_MCP_REPO=myrepo \
  python3 github-mcp-proxy.py
# Prints the listening port to stdout
```

### Configuration

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

### GitHub App token generation

Two helper scripts generate repo-scoped tokens via a GitHub App:

- **`github_app_token.sh`** — Pure bash. Generates installation tokens from a GitHub App private key.
- **`github_app_token_demo.py`** — Python. Supports device flow (OAuth) for user tokens, with caching.

See the scripts for usage instructions.

## How it works

1. **`claude-vm-setup`** creates a Debian 13 VM with Lima, installs dev tools + Chrome + Claude Code, and stops it as a reusable template
2. **`claude-vm [args]`** clones the template, mounts your working directory read-write, runs optional `.claude-vm.runtime.sh`, then launches Claude with full permissions (forwarding any arguments to the `claude` command)
3. On exit, the cloned VM is stopped and deleted. The template persists for reuse

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
| AI | Claude Code, Chrome DevTools MCP server |

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
