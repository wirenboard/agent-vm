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
- Phase 2 (launcher MVP): pending.

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

Actually running a sandbox requires the microsandbox runtime prerequisites
(Linux with KVM, or macOS Apple Silicon). See microsandbox's
[`README`](vendor/microsandbox/README.md).
