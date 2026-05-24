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

- Linux with `/dev/kvm` (rw) — your user must be in the `kvm`
  group: `sudo usermod -aG kvm $USER` and re-login.
- Node.js 18+ (already there if you use Claude Code / Codex CLI /
  OpenCode — they're all npm-distributed).

`~/.microsandbox/lib/libkrunfw.so.5.x` auto-installs on first
launch.

## Quick start

```bash
npm install -g @wirenboard/agent-vm        # or: npx @wirenboard/agent-vm <cmd>

agent-vm setup            # pulls the latest image from ghcr.io and verifies it boots

cd ~/your-project
agent-vm claude           # or codex / opencode / shell
```

The npm package bundles a prebuilt `agent-vm` binary, the patched
`msb`, and libkrunfw. agent-vm finds them via
`current_exe()`-relative paths, so a user's separate
`~/.microsandbox/bin/msb` (if any) never shadows the patched build.

## Build from source

```bash
git clone -b rewrite-microsandbox https://github.com/wirenboard/agent-vm
cd agent-vm
git submodule update --init vendor/microsandbox
sudo apt-get install -y libcap-ng-dev libdbus-1-dev pkg-config
cargo build --release -p agent-vm
cargo build --release --manifest-path vendor/microsandbox/Cargo.toml \
    -p microsandbox-cli --bin msb
./target/release/agent-vm setup       # uses the locally-built msb sibling
```

`agent-vm setup` pulls
`ghcr.io/wirenboard/agent-vm:latest` by default; pass
`--image localhost:5000/agent-vm:latest` to use a local image
you've built via `images/build.sh`.

## Subcommands

```
claude | codex | opencode | shell   launch an agent in a per-project sandbox
pull                                refresh the cached image
setup                               pull base image + verify boot
clipboard {get,put} [--sys]         exchange a string with the project sandbox
```

## Image release cadence

The base OCI image (`ghcr.io/wirenboard/agent-vm:latest`) is
rebuilt hourly by CI, picking up the latest Claude Code, Codex CLI,
and OpenCode releases automatically. Pin a specific build with
`--image ghcr.io/wirenboard/agent-vm:YYYY-MM-DDTHH` (date tags are
immutable; the last 14 days are retained).

The agent-vm binary and the image are version-locked through an
**image-API-version** integer
(`/etc/agent-vm-image-version` inside the image). Mismatch → clean
error at launch instead of mysterious in-VM failures.

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
