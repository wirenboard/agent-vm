# agent-vm (microsandbox rewrite)

Work-in-progress rewrite of [`agent-vm`](https://github.com/wirenboard/agent-vm)
on top of [microsandbox](https://github.com/wirenboard/microsandbox). The goal
is sub-second cold-start microVMs for AI coding agents (Claude Code, Codex CLI,
OpenCode) with host-rooted credentials and no in-VM proxy.

This is a long-running rewrite. The current state lives on
`rewrite-microsandbox`. The original `agent-vm` continues to live on `main`
until v1 of the rewrite ships.

## Where to look

- [`PLAN.md`](PLAN.md) — phased roadmap, what's in/out of v1.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — running record of design decisions,
  filled in as each phase lands.

## Status

- Phase 0 (scaffolding): done.
- Phase 1 (base OCI image + `agent-vm setup`): done.
- Phase 2 (launcher MVP — `claude`/`codex`/`opencode`/`shell`): done; live
  API smoke deferred to Phase 3.
- Phase 2.x (RUST_LOG, host-path mounting, pull progress bar,
  `agent-vm pull` + update-available banner, registry auto-recovery):
  done.
- Phase 3 (host-rooted secrets via microsandbox TLS intercept): done.
  Real Claude/Codex tokens stay on the host; the in-VM agent only sees
  placeholders. See ARCHITECTURE.md "Phase 3" for the two-layer
  placeholder dance.
- Phase 4 (token-refresh semantics + `msb` rebuild for
  `SecretValue::File`): **next**.
- Phase 5 (fast-launch via detached mode): deferred — see PLAN.md;
  snapshots don't help launch time, detached mode is a product-shape
  decision.
- Phase 6 (distribution + polish + docs): pending.

## Building

```bash
git submodule update --init vendor/microsandbox
cargo build -p agent-vm
```

## Building the base image

```bash
./target/debug/agent-vm setup        # build + verify in a throwaway sandbox
./target/debug/agent-vm setup --no-verify
```

Requires Docker on the host. A `registry:2` container named
`agent-vm-registry` will be created on first run and bound to
`127.0.0.1:5000`.

## Running an agent

```bash
cd your/project
agent-vm claude        # or codex / opencode / shell
agent-vm claude -- -p "fix the lint errors"  # forward args to the agent

# Smoke / scripted use:
agent-vm shell -- -c 'ls /workspace && claude --version'
```

The first arg after the subcommand is treated as input to the agent. Use
`--` if any forwarded arg starts with `-` (otherwise clap will try to claim
it). Project state lives in `~/.local/state/agent-vm/<hash>/` and survives
between launches. Phase 2 supports auth via the `ANTHROPIC_API_KEY` /
`OPENAI_API_KEY` env vars only; the OAuth-refresh path lands in Phase 3.

Actually running a sandbox requires the microsandbox runtime prerequisites
(Linux with KVM, or macOS Apple Silicon). See microsandbox's
[`README`](vendor/microsandbox/README.md).
