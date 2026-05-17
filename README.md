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

Phase 0 (scaffolding): in progress.

## Building

```bash
git submodule update --init vendor/microsandbox
cargo check -p agent-vm
```

Actually running a sandbox requires the microsandbox runtime prerequisites
(Linux with KVM, or macOS Apple Silicon). See microsandbox's
[`README`](vendor/microsandbox/README.md).
