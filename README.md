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
| `--disk GB` | VM disk size in GB | 30 |
| `--memory GB` | VM memory ceiling in GB | 16 (Linux), 4 (macOS) |

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
agent-vm claude --memory 8                       # Start with 8G instead of default 2G
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

### Dynamic memory (Linux)

On Linux, VMs use a [virtio-balloon](https://www.linux-kvm.org/page/Projects/auto-ballooning) device to dynamically adjust memory. The VM is created with a 16G ceiling but starts with only 2G of usable RAM. As the guest needs more memory, the balloon daemon automatically deflates to give it more, up to the full 16G. When memory pressure drops, unused memory is reclaimed back to the host.

This means you can run multiple VMs without each one reserving its full allocation upfront.

```bash
agent-vm claude --memory 5 --max-memory 10   # Start with 5G, grow up to 10G
agent-vm claude --memory 8                   # Start with 8G, ceiling from template (16G)
agent-vm memory                              # Show current memory of all running VMs
agent-vm memory 12G                          # Manually set memory to 12G (auto-detect VM)
agent-vm memory my-vm 8G                     # Set specific VM's memory
```

On macOS (Apple Silicon with VZ backend), QEMU is not used and balloon is not available. VMs use a fixed 4G memory allocation instead. You can still use `--memory` to set a different fixed size. The `agent-vm memory` subcommand (live adjustment) is Linux-only.

### Common options

| Flag | Applies to | Description |
|------|-----------|-------------|
| `--memory GB` | claude, opencode, shell | Initial memory (default: 2G with balloon, 4G without) |
| `--max-memory GB` | claude, opencode, shell | Memory ceiling for balloon (default: from template) |
| `--no-git` | claude, opencode, shell | Skip GitHub integration |
| `--usb DEVICE` | claude, opencode, shell | Pass USB device to VM (repeatable) |

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
3. Ephemeral config (`CLAUDE.md`) stays in-VM and is not persisted
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

## Credential Proxy

A two-layer proxy chain keeps all API credentials out of the VM:

1. **mitmproxy** (inside VM, port 8080) — transparently intercepts HTTPS traffic to configured domains, redirects it to the host-side credential proxy. Requests to non-configured domains pass through unchanged. Requests to blocked domains (e.g. `datadoghq.com`) are rejected with 403.

2. **credential-proxy.py** (on host) — receives redirected requests, injects real auth headers based on domain/path matching rules, and forwards to the real upstream. Supports per-repo GitHub tokens via path-prefix matching.

The VM only ever sees placeholder tokens. Real credentials live in the host process's memory. A per-instance shared secret (via standard `Proxy-Authorization`) prevents cross-VM credential theft.

### Configuration

| Env var | Description | Default |
|---------|-------------|---------|
| `CLAUDE_VM_PROXY_ACCESS_TOKEN` | Anthropic API token to inject | — |
| `AI_HTTPS_PROXY` | Upstream proxy for AI API traffic only (e.g. `http://user:pass@host:8082`) | — |
| `AI_SSL_CERT_FILE` | Extra CA cert PEM for `AI_HTTPS_PROXY` | — |
| `CREDENTIAL_PROXY_DEBUG` | Set to `1` for verbose logging | `0` |
| `CREDENTIAL_PROXY_LOG_DIR` | Directory for log file | `.` |

When `AI_HTTPS_PROXY` is set, only AI API requests (`api.anthropic.com`) are routed through it. GitHub and other traffic goes direct.

## GitHub Integration

When you run `agent-vm claude` or `agent-vm opencode` inside a git repo with a GitHub remote, it automatically:

1. Detects the repository (and submodules) from `git remote`
2. Checks push access via `git push --dry-run`
3. Obtains repo-scoped GitHub tokens via the device flow (browser-based OAuth)
4. Configures the credential proxy with per-repo path-prefix rules
5. Rewrites SSH URLs to HTTPS so all git traffic goes through mitmproxy
6. Writes instructions to `~/.claude/CLAUDE.md` in the VM so the agent knows git is available

No credentials are ever exposed to the VM. The credential proxy injects tokens on the host side based on the request path (e.g. `/owner/repo` for git, `/repos/owner/repo` for the GitHub API).

### Token generation and scoping

Tokens are generated via a [GitHub App](https://docs.github.com/en/apps) using the [device flow](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#device-flow):

1. **One-time setup**: Create a GitHub App with `contents: write` permission and install it on your org/account. The App's Client ID is configured in `claude-vm.sh`.
2. **Per-session**: The device flow runs for each repo that has push access — you approve in a browser.
3. **Multi-repo**: Each repo gets its own scoped token. The credential proxy uses path-prefix matching to inject the right token per request.
4. **Caching**: Tokens are cached in `~/.cache/claude-vm/` and automatically refreshed when expired.

## How it works

1. **`agent-vm setup`** creates a Debian 13 VM with Lima, installs dev tools + Chrome + Claude Code + OpenCode, and stops it as a reusable template
2. **`agent-vm claude [args]`** clones the template, mounts your working directory read-write, starts the balloon daemon (Linux), runs optional `.claude-vm.runtime.sh`, then launches Claude with full permissions (forwarding any arguments to the `claude` command)
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
| Memory | virtio-balloon auto-scaling (Linux only) |

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
