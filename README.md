# agent-vm (microsandbox rewrite)

Run AI coding agents (Claude Code, Codex CLI, OpenCode) inside per-project
microVMs that boot in ~2 seconds, with the host's real OAuth tokens **never
entering the VM** and a per-launch GitHub-repo allow-list for `git
push` / `gh pr create`.

This is the in-progress rewrite of
[`wirenboard/agent-vm`](https://github.com/wirenboard/agent-vm) on top of
[microsandbox](https://github.com/wirenboard/microsandbox). The original Bash
implementation continues to live on `main`; the rewrite lives on
`rewrite-microsandbox`.

## Why

The original `agent-vm` is mature but heavy: ~30 s cold start, 16 GB disk
template, host-side mitmproxy chain, balloon daemon, custom GitHub App. This
rewrite uses microsandbox's libkrun-backed microVMs and built-in TLS-intercept
+ placeholder-substituted secrets. ~2 s launches, ~17 MB runtime, no host
proxy, and the "real tokens never enter the VM" guarantee is enforced at the
network layer instead of by a Python middlebox.

## Status

Phases 0–7 + 9 are done. Each phase is one PR; see [PLAN.md](PLAN.md) for the
running roadmap.

- 0–2.x: scaffolding, image, launcher MVP, polish.
- 3–4: host-rooted Claude / Codex credentials with OAuth-refresh support.
- 5: OpenCode auth + host-cred mutation snapshot.
- 6: gh / git credential injection with per-launch repo allow-list at the
  proxy.
- 7: `--mount`, clipboard bridge, `ccusage` wrapper, Chrome DevTools MCP.
- 8: fast-launch via detached mode (deferred — product-shape decision).
- 9: distribution + polish + docs.

Verified end-to-end on Linux/KVM against real Claude (Anthropic) and Codex
(ChatGPT OAuth) accounts.

## Requirements

- Linux with KVM (`/dev/kvm` readable/writable) — primary target. macOS-VZ
  works at the microsandbox level but isn't exercised here.
- Docker, for building the base OCI image and running the local registry.
- A Rust toolchain (rustup stable) to build agent-vm + the patched `msb`.
- `libcap-ng-dev` and `libdbus-1-dev` on the build host (transitive deps
  of the microsandbox crate).

The microsandbox runtime libs (`~/.microsandbox/{bin/msb,
lib/libkrunfw.so.5.x}`) are auto-installed on first launch — you don't
need to download them by hand.

## Build

```bash
git clone https://github.com/wirenboard/agent-vm
cd agent-vm
git checkout rewrite-microsandbox
git submodule update --init vendor/microsandbox
sudo apt-get install -y libcap-ng-dev libdbus-1-dev pkg-config
cargo build --release -p agent-vm
BIN=$(pwd)/target/release/agent-vm
```

`agent-vm setup` builds the base image (`localhost:5000/agent-vm:latest`),
the patched `msb` from `vendor/microsandbox`, and verifies the image by
booting a throwaway sandbox:

```bash
"$BIN" setup
```

## Use

```bash
cd ~/your-project
"$BIN" claude        # Claude Code TUI
"$BIN" codex         # Codex CLI TUI
"$BIN" opencode      # OpenCode TUI
"$BIN" shell         # bash inside the sandbox

# One-shots (no TTY, output streamed live):
"$BIN" claude -p "fix the lint errors"
"$BIN" codex exec --skip-git-repo-check "Reply OK"
"$BIN" shell -- -c 'cargo test'
```

All four subcommands accept:

- `--memory N` — VM memory in GiB (default 2)
- `--cpus N` — vCPUs (default 2)
- `--image REF` — override the OCI image
- `--no-update-check` — skip the registry HEAD on launch
- `--no-git` — opt out of gh / git injection
- `--repo OWNER/NAME` — add to the GitHub allow-list (repeatable)
- `--mount HOST[:GUEST]` — extra bind mount (subject to libkrun IRQ cap)

Plus:

- `"$BIN" pull` — fetch the latest image into the microsandbox cache.
- `"$BIN" clipboard {get,put} [--sys]` — exchange a string with the
  per-project sandbox via `<state>/clipboard.txt` (mounted at
  `/agent-vm-state/clipboard.txt` in the guest). `--sys` also pulls from
  / pushes to the host system clipboard.
- `bin/agent-vm-ccusage` — wrapper that unions `~/.claude` and every
  per-project session dir before running `npx ccusage@latest`.

## Credentials

- **Claude**: reads `~/.claude/.credentials.json`. Sends a placeholder
  bearer from the guest; the proxy swaps to the real token on outbound
  api.anthropic.com traffic. OAuth refresh is MITM'd into a host-side
  `claude -p` invocation so long sessions survive token rotation.
- **Codex**: reads `~/.codex/auth.json`. Same model, with OpenAI's
  `id_token` JWT replaced with an alg-none placeholder so no PII enters
  the guest.
- **OpenCode**: synthesizes an OpenCode-shaped `auth.json` from the host
  Codex credentials (same OpenAI account); a synthetic placeholder JWT
  goes into `tokens.openai.access` and is substituted on the wire.
- **gh / git**: if `gh auth status` shows you're logged in, the host
  token is injected into the guest's `~/.gitconfig` and
  `~/.config/gh/hosts.yml` as a placeholder. The proxy substitutes for
  the real bearer on outbound traffic to GitHub.
  - **Per-launch repo allow-list.** `git remote -v` of the cwd gives the
    initial set; `--repo OWNER/NAME` widens it; api.github.com requests
    outside the list get a synthesized 403 from the proxy hook.

Real tokens live in `${XDG_STATE_HOME}/agent-vm/<hash>.secrets/` (mode 0700)
on the host, **outside** the bind mount the guest sees. A SHA-256 snapshot
of `~/.claude/.credentials.json`, `~/.codex/auth.json`, and
`~/.local/share/opencode/auth.json` is taken at launch and re-checked on
exit; any mutation outside the Phase 4 refresh path prints a warning.

## Project hook

If the project root contains an executable `.agent-vm.runtime.sh`, the
launcher `source`s it inside the guest at the project's bind path before
exec'ing the agent. Use it for `npm install`, env exports, dev-server
startup, etc.

```sh
# .agent-vm.runtime.sh
set -e
npm install --silent
export FOO=bar
```

## Troubleshooting

- **`RegisterNetDevice(IrqsExhausted)` on boot:** libkrun's IRQ pool is
  tight. Drop a `--mount` or pass `--no-git`.
- **`runtime error: handshake read id_offset: timed out`:** check
  `free -h` — boot needs more memory than the host has free. Try
  `--memory 1`.
- **Codex hangs at "Reading additional input from stdin...":** kill,
  re-run. The launcher forces stdin to `/dev/null` in non-TTY mode;
  if you piped real input, codex consumed it before printing the
  banner.
- **`gh api` returns "Bad credentials" but the same call works on
  host:** you're in a doubly-nested setup where an *outer* agent-vm
  bridge replaced your host token with its own placeholder. The
  inner substitution is working; the outer bridge isn't
  re-substituting on the nested VM's egress. Run from a non-nested
  host.

## Docs

- [PLAN.md](PLAN.md) — phased roadmap with what landed when and what's
  still open.
- [ARCHITECTURE.md](ARCHITECTURE.md) — per-phase design notes; the
  source of truth for *why* something looks the way it does.
