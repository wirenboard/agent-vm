# @wirenboard/agent-vm

Sandboxed VMs for AI coding agents — Claude Code, Codex CLI, OpenCode
— running inside per-project libkrun microVMs built on
[microsandbox](https://github.com/wirenboard/microsandbox).

This package is a thin launcher; the actual native binaries
(`agent-vm`, the patched `msb`, libkrunfw) ship in the
platform-specific subpackage installed automatically as an
`optionalDependency` (e.g. `@wirenboard/agent-vm-linux-x64`).

## Install

```bash
npm install -g @wirenboard/agent-vm
# or
npx @wirenboard/agent-vm <subcommand>
```

Requirements: Linux with `/dev/kvm` (your user must be in the `kvm`
group) and Node 18+. macOS and Windows aren't supported yet.

## Quick start

```bash
agent-vm setup            # pull the latest image, verify it boots
cd ~/your-project
agent-vm claude           # or codex / opencode / shell
```

Full docs, subcommand reference, and source:
<https://github.com/wirenboard/agent-vm>.
