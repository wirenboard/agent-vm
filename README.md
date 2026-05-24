# agent-vm

Run Claude Code / Codex / OpenCode inside a per-project libkrun microVM,
booting in ~2 seconds, with:

- **Host OAuth tokens never enter the VM.** The TLS-intercept proxy in
  [microsandbox](https://github.com/wirenboard/microsandbox) substitutes
  the real bearer for a placeholder on the way out. OAuth refresh is
  MITM'd so multi-hour sessions survive token rotation.
- **Per-launch GitHub repo allow-list.** Auto-detected from
  `git remote -v`; extend with `--repo OWNER/NAME`. `gh pr create`,
  `git push` etc. are filtered at the proxy — off-list calls get a 403
  before they reach GitHub.
- **Sandbox is the boundary.** Root inside the VM, project bind-mounted
  at its host path, `--dangerously-skip-permissions` set by default
  (the microVM is the only thing actually keeping the agent on rails).

This is the Rust rewrite of the original Bash
[`wirenboard/agent-vm`](https://github.com/wirenboard/agent-vm) on
top of microsandbox. Living on `rewrite-microsandbox` until v1.

## Requirements

- Linux with `/dev/kvm` (rw)
- Docker, for building the base image + running the local registry
- Rust toolchain (rustup stable)
- `libcap-ng-dev`, `libdbus-1-dev`, `pkg-config`

`~/.microsandbox/{bin/msb, lib/libkrunfw.so.5.x}` auto-install on
first launch.

## Quick start

```bash
git clone -b rewrite-microsandbox https://github.com/wirenboard/agent-vm
cd agent-vm
git submodule update --init vendor/microsandbox
sudo apt-get install -y libcap-ng-dev libdbus-1-dev pkg-config
cargo build --release -p agent-vm
BIN=$(pwd)/target/release/agent-vm

"$BIN" setup                    # build + push image, build patched msb

cd ~/your-project
"$BIN" claude                   # or codex / opencode / shell
```

## Subcommands

```
claude | codex | opencode | shell   launch an agent in a per-project sandbox
pull                                refresh the cached image
setup                               build base image + patched msb
clipboard {get,put} [--sys]         exchange a string with the project sandbox
```

Each launcher accepts:

| flag | what |
|---|---|
| `--memory N` | VM memory GiB (default 2) |
| `--cpus N` | vCPUs (default 2) |
| `--image REF` | override the OCI image |
| `--no-update-check` | skip the registry HEAD on launch |
| `--no-git` | skip gh/git auth injection (still respects `--repo`) |
| `--repo OWNER/NAME` | add to the GitHub allow-list (repeatable) |
| `--mount HOST[:GUEST]` | extra bind mount (subject to libkrun IRQ cap) |

Trailing args go to the agent: `agent-vm claude -p "say hi"`,
`agent-vm shell -- -c 'cargo test'`.

## Credentials

Reads from the host:

- `~/.claude/.credentials.json` (Claude)
- `~/.codex/auth.json` (Codex, OpenCode)
- `gh auth token` (git/gh)

The guest gets placeholder strings; the proxy substitutes on the wire.
Real tokens live in `${XDG_STATE_HOME}/agent-vm/<hash>.secrets/` (0700)
on the host, **outside** the bind mount the guest sees. A SHA-256
snapshot of the three credential files is taken at launch and
re-checked on exit; unexpected mutations print a warning.

For Claude/Codex, when the in-VM agent's bearer expires the
hook MITMs the OAuth refresh, runs `claude -p`/`codex exec` on the
host to rotate, and feeds the new placeholder back to the guest — no
re-attach required.

## Project hook

If the project root contains an executable `.agent-vm.runtime.sh`,
the launcher sources it inside the guest before exec'ing the agent.
Use for `npm install`, env exports, dev-server startup. Non-zero
exit aborts the launch.

## Troubleshooting

- **`RegisterNetDevice(IrqsExhausted)` at boot** — libkrun's virtio
  IRQ pool is saturated by project + state + net + secrets. Drop a
  `--mount` or pass `--no-git`.
- **`handshake read id_offset: timed out`** — `free -h`; the VM needs
  more memory than is available. Try `--memory 1`.
- **GitHub 403 from the proxy** — repo isn't in the allow-list.
  Pass `--repo OWNER/NAME` or run from a project with the right
  remote.

## See also

- [PLAN.md](PLAN.md) — phased roadmap, what's done, what's deferred.
- [ARCHITECTURE.md](ARCHITECTURE.md) — design notes; why things look
  the way they do.
