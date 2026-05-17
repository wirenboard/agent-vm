# agent-vm rewrite — PLAN

Living roadmap for rewriting `agent-vm` on top of
[microsandbox](https://github.com/wirenboard/microsandbox). The plan is locked
*now* but is updated as phases land. Each phase ends at a stop point so we can
inspect, adjust, then proceed.

The architecture details and design rationale live in `ARCHITECTURE.md` and are
written after each phase, not up front.

## Why a rewrite

The existing `agent-vm` (Bash, 2.4kloc + Python helpers, Lima full VMs) is
mature but heavy: 30-second cold start, 16 GB disk template, host-side `mitm`
chain, balloon daemon, custom GitHub App. microsandbox boots microVMs in
~100 ms from OCI images, has a first-class Rust SDK, and ships TLS interception
+ placeholder-substituted secrets at the network layer. Most of `agent-vm`'s
infrastructure becomes either unnecessary or moves into a small Rust binary.

## v1 scope (in)

- Subcommands: `setup`, `claude`, `codex`, `opencode`, `shell`.
- Project working directory mounted into the sandbox as `/workspace`.
- Per-project session persistence for `~/.claude/`, `~/.codex/`,
  `~/.local/share/opencode/` under `${XDG_STATE_HOME}/agent-vm/<project-hash>/`.
- Host-rooted credentials with refresh: real tokens never enter the VM; host
  `claude -p` / `codex exec` are used to rotate; the VM picks up the new token
  on the next request without restarting the sandbox.
- Pre-baked Debian-based OCI image with the three agent CLIs and dev tools.
- Interactive attach for the agent TUIs.

## v1 scope (out, may revisit)

- GitHub App device flow + per-repo scoped tokens.
- USB passthrough.
- Dynamic memory / balloon daemon.
- Clipboard bridge.
- `ccusage` wrapper.
- Chrome DevTools MCP wiring.
- GitHub Copilot API token acquisition.
- `--mount` for extra host directories.
- `AI_HTTPS_PROXY` upstream proxy chaining.
- Apple Silicon / macOS-VZ specifics.
- WSL2-on-Windows specifics.

## Phased roadmap

Each row is one PR; we stop after each phase, fill in `ARCHITECTURE.md`, then
the user signs off on the next.

### Phase 0 — Scaffolding

- Worktree on `rewrite-microsandbox`.
- microsandbox added as a git submodule at `vendor/microsandbox` (tracking
  `wirenboard/microsandbox @ main`; we'll branch off here in Phase 3).
- Cargo workspace at the worktree root; `crates/agent-vm/` binary crate.
- Hello-world `main.rs`: `Sandbox::builder("hello").image("alpine").create()`,
  run `echo`, stop.
- `cargo check -p agent-vm` succeeds (runtime exec needs KVM and is out of
  scope for the lint).

**Done when:** scaffold compiles, PLAN and ARCHITECTURE files exist, submodule
is registered.

### Phase 1 — OCI image

- `images/Dockerfile`: Debian 13 base + `git curl wget jq build-essential
  python3 python3-pip nodejs(22) ripgrep fd-find docker-cli` + the three agent
  CLIs (`@anthropic-ai/claude-code`, `opencode-ai`, `@openai/codex`).
- `images/build.sh` builds locally and tags `agent-vm:latest`. No registry
  push in v1.
- `agent-vm setup` subcommand wraps `images/build.sh`.

**Done when:** `agent-vm setup` builds the image and `msb run agent-vm:latest
-- claude --version` succeeds.

### Phase 2 — Launcher MVP

- clap-based subcommand parser: `setup | claude | codex | opencode | shell`.
- Project hash + state dir helper (`${XDG_STATE_HOME:-~/.local/state}/agent-vm
  /<hash>/`).
- Mount `cwd` at `/workspace` inside the sandbox.
- Symlink session dirs from the persisted state dir into the guest home.
- `attach_shell` for interactive agents.
- Credentials: env-var only (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`) so we can
  test the launcher end-to-end without the refresh machinery.

**Done when:** `cd repo && agent-vm claude -p "say hi"` returns a real Claude
response from inside the sandbox.

### Phase 3 — Static host-rooted secrets

- Branch `vendor/microsandbox` to add a `SecretValue::File(PathBuf)` variant
  alongside the existing `String` value. The TLS-intercept proxy re-reads the
  file on every substitution.
- `agent-vm` on startup snapshots host `~/.claude/.credentials.json` and
  `~/.codex/auth.json` into `<state>/tokens/{anthropic,openai}.token` files.
- Register file-backed secret entries for `api.anthropic.com`,
  `api.openai.com`, `chatgpt.com`, `platform.claude.com`, etc.
- Cross-instance lock so two `agent-vm` processes don't fight over the token
  files.

**Done when:** inside the guest, `cat /proc/$$/environ | tr '\0' '\n' | grep
ANTHROPIC` shows only placeholders, while a real Claude request succeeds.

### Phase 4 — Refresh semantics

- inotify (Linux) / kqueue (macOS) watcher on host creds files: when host
  Claude/Codex rotates tokens, we re-snapshot to the file microsandbox watches.
- Proactive expiry watch: when the access token is < 5 minutes from expiry
  and no host activity has refreshed it, spawn `claude -p "ping" --model
  sonnet` / `codex exec --skip-git-repo-check "Reply OK"` on the host.
- Single-flight per credential; cross-instance lockfile so concurrent
  `agent-vm` instances don't all kick off a refresh at once.

**Done when:** a multi-hour session crosses a token rotation without the agent
seeing an auth error.

### Phase 5 — Polish & docs

- `.agent-vm.runtime.sh` project hook.
- `--memory N` flag (passed through to microsandbox builder).
- Full arg passthrough to the agent command.
- README rewrite: install, setup, usage, troubleshooting.
- Smoke tests: at least one end-to-end test that boots the image and runs a
  trivial agent command.

**Done when:** README is publishable; one CI smoke test green.

## Working agreements

1. **One phase = one PR.** Stop after each.
2. **ARCHITECTURE.md is the source of truth for the *why*.** Every major
   design choice in a phase gets a short subsection: what was chosen, what was
   rejected, why.
3. **Don't touch old `agent-vm`** (Bash, Python helpers) on the rewrite
   branch. The old tree stays on `main` until v1 is shipped from the new
   branch.
4. **microsandbox changes go into the submodule, not vendored copies.** If we
   need to fork, we do it on a branch of `wirenboard/microsandbox` so the
   diff stays reviewable upstream.
