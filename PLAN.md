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
the user signs off on the next. Per-phase status updates land in this file as
each phase ships.

### Phase 0 — Scaffolding [done — commits `cb4be40`, `4462180`]

- Worktree on `rewrite-microsandbox`.
- microsandbox added as a git submodule at `vendor/microsandbox` (tracking
  `wirenboard/microsandbox @ main`; we'll branch off here in Phase 3).
- Cargo workspace at the worktree root; `crates/agent-vm/` binary crate.
- Hello-world `main.rs`: `Sandbox::builder("hello").image("alpine").create()`,
  run `echo`, stop.
- `cargo check -p agent-vm` succeeds.

**Done when:** scaffold compiles, PLAN and ARCHITECTURE files exist, submodule
is registered. Verified end-to-end on KVM: 2.7 s round-trip for boot/exec/
teardown with the alpine image cached.

### Phase 1 — OCI image [done — commit `d23c421`]

- `images/Dockerfile`: Debian 13 slim + `ca-certificates curl wget git jq bash
  python3 python3-pip ripgrep fd-find nodejs(22)` + the three agent CLIs
  (`claude.ai/install.sh`, `opencode.ai/install`, `openai/codex install.sh`).
  Deliberately minimal: no Docker, Chromium, LSP plugins, mitmproxy, gh,
  Copilot — those stay deferred per the v1-scope-out list.
- `images/build.sh` ensures a host-local `registry:2` container
  (`agent-vm-registry` on `127.0.0.1:5000`) is running, then builds and pushes
  `localhost:5000/agent-vm:latest`. (The original plan said "no registry push";
  changed to registry push because microsandbox's image-cache and snapshot
  semantics are keyed off OCI references — see ARCHITECTURE.md "Image
  distribution" for the rationale.)
- `agent-vm setup` is a clap subcommand that shells out to `images/build.sh`,
  then verifies the freshly pushed image by booting it under microsandbox and
  running `claude --version && opencode --version && codex --version`.
  `--no-verify` and `--image`/`AGENT_VM_IMAGE_TAG` escape hatches included.

**Done when:** `agent-vm setup` builds the image and the verify sandbox
reports the three agent versions. Result: Claude 2.1.143, OpenCode 1.15.3,
codex-cli 0.130.0.

### Phase 2 — Launcher MVP [pending]

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

### Phase 3 — Static host-rooted secrets [pending]

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

### Phase 4 — Refresh semantics [pending]

- inotify (Linux) / kqueue (macOS) watcher on host creds files: when host
  Claude/Codex rotates tokens, we re-snapshot to the file microsandbox watches.
- Proactive expiry watch: when the access token is < 5 minutes from expiry
  and no host activity has refreshed it, spawn `claude -p "ping" --model
  sonnet` / `codex exec --skip-git-repo-check "Reply OK"` on the host.
- Single-flight per credential; cross-instance lockfile so concurrent
  `agent-vm` instances don't all kick off a refresh at once.

**Done when:** a multi-hour session crosses a token rotation without the agent
seeing an auth error.

### Phase 5 — Polish & docs [pending]

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
5. **Every phase updates three docs together.** PLAN.md gets the status
   marker and any plan corrections. ARCHITECTURE.md gets the new design
   subsection. README.md status list moves the phase from pending to done.
   The commit message references the phase number.
