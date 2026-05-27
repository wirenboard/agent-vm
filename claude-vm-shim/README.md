# claude-vm-shim

**Status: working prototype.** End-to-end sessions run inside an agent-vm
sandbox. Verified: `claude --bg "what is 2+2"` boots a VM, the in-VM claude
adopts the session, calls api.anthropic.com (auth via `/agent-vm-state` mount),
generates a session name ("math calculation"), streams output back to
`claude logs <id>`, and respects `claude stop <id>`. See "Known gaps" for the
parts that still need work.

Run every Claude Code "remote control" session in its own `agent-vm` sandbox by
intercepting the daemon's per-session `--bg-spare` exec and forwarding the
session's UDS protocol into a freshly-booted VM.

## Why this exists

`claude remote-control` runs a persistent host-side daemon that accepts session
requests from the mobile app / claude.ai/code. Each new session spawns a
two-process tree on the host:

```
claude --bg-pty-host <ptySock> 200 50 -- claude --bg-spare <claimSock>
                  └── PTY supervisor                  └── the session worker
```

The "bg-spare" worker is the actual session — it binds the claim socket,
the daemon connects to it, and from then on it's a normal Claude Code session
running on the host's filesystem.

This project intercepts the per-session exec and reroutes it into an
`agent-vm` sandbox. The daemon still thinks it's talking to a local
`claude --bg-spare`; underneath, it's talking to one running inside a VM.

## Architecture

```
   ┌─────────────────── host ────────────────────┐    ┌───── agent-vm guest ─────┐
   │ daemon                                       │    │                          │
   │   │ posix_spawn(claude --bg-spare X.sock)    │    │                          │
   │   ▼                                          │    │                          │
   │ libclaude_vm_shim.so  ── rewrites argv ──┐   │    │                          │
   │                                          │   │    │                          │
   │ claude-vm-dispatcher  ◀──────────────────┘   │    │                          │
   │   ├─ bind UDS at X.sock        ◀───── daemon │    │ claude --bg-spare        │
   │   ├─ bind TCP 0.0.0.0:N                      │    │   /run/claude-vm/      ▲ │
   │   └─ spawn agent-vm shell ───────────────────┼────┤   bg-spare.sock        │ │
   │                                              │    │                        │ │
   │ accept UDS ◀── daemon connects               │    │ claude-vm-bridge       │ │
   │ accept TCP ◀────── via host's private IP ────┼────┤   ↑ poll /run/...sock  │ │
   │                                              │    │   ↑ TCP→host:N         │ │
   │ bidi-relay UDS ↔ TCP                         │    │                        │ │
   └──────────────────────────────────────────────┘    └────────────────────────┘
```

Three executable artifacts live in `/opt/claude-vm-shim/`:

- `lib/libclaude_vm_shim.so` — `LD_PRELOAD`-able cdylib that intercepts
  `execve`/`execvp`/`posix_spawn`/`posix_spawnp`. When argv matches
  `[<claude>, "--bg-spare", <sock>]`, it rewrites the call to invoke the
  dispatcher with the original argv as its arguments.
- `bin/claude-vm-dispatcher` — handles a single `--bg-spare` invocation:
  binds the host UDS, binds a TCP listener, boots `agent-vm`, accepts both
  sides, and bidi-relays bytes. Falls back to a local `claude` exec on
  failure or when `CLAUDE_VM_SHIM_PASSTHROUGH=1`.
- `bin/claude-vm-bridge` — runs inside the VM. Polls for the in-VM UDS that
  `claude --bg-spare` creates, opens TCP to the dispatcher via the host's
  private IP, bidi-relays bytes.

A `bin/claude-shimmed` wrapper script sets `LD_PRELOAD` and execs the real
`claude` binary, so the daemon and every subprocess it spawns inherit the
interception.

### Why TCP-over-private-IP instead of vsock or the gateway IP

- **vsock**: not exposed to the guest by microsandbox today; would require a
  `VmBuilder::vsock(true)` call inside the runtime.
- **gateway IP**: microsandbox's network stack rewrites guest connections to
  the gateway IP into host loopback. Cleaner, but the default policy only
  allows DNS to the "host" group — any other port is denied.
- **host's private IP**: falls in the "private" destination group, which is
  also denied by the default `public_only` policy. We added an opt-in
  patch to `agent-vm` that flips the policy to `non_local()` when
  `AGENT_VM_ALLOW_LOCAL_EGRESS=1`, which allows TCP egress to RFC1918 /
  CGNAT / ULA addresses. The dispatcher sets this env var on each spawn.

## Layout

```
claude-vm-shim/
├── Cargo.toml             # workspace
├── shim/                  # libclaude_vm_shim.so   (cdylib, LD_PRELOAD)
├── dispatcher/            # claude-vm-dispatcher    (bin, host)
├── bridge/                # claude-vm-bridge        (bin, guest)
├── wrapper/
│   └── claude-wrapper.sh  # parameterized; install.sh bakes paths
├── install.sh             # installs to /opt/claude-vm-shim/
└── README.md
```

The project lives at the top level of agent-vm (not under `crates/`) because
it has a separate lifecycle and may move to its own repo. The one tie-in is a
small patch to `crates/agent-vm/src/run.rs` (~14 lines) that adds the
`AGENT_VM_ALLOW_LOCAL_EGRESS` escape hatch.

## Building

Prerequisites:

- `gcc` + `libc6-dev` + `libcap-ng-dev` + `libdbus-1-dev` + `pkg-config`
- `npm install -g @wirenboard/agent-vm` (provides bundled `msb` + `libkrunfw`)
  and `agent-vm setup --no-verify` to pull the OCI image
- The repo's vendored Rust toolchain (use `PATH=$REPO/.agent-vm-rust/cargo/bin:$PATH`
  and `RUSTUP_HOME`/`CARGO_HOME` pointing at `$REPO/.agent-vm-rust/`)
- The `vendor/microsandbox` submodule. If you cloned this branch as a
  worktree from a checkout that already has it, symlink to the existing
  copy rather than re-cloning:

    ```sh
    rmdir vendor/microsandbox
    ln -sfn /path/to/primary-checkout/vendor/microsandbox vendor/microsandbox
    ```

    Otherwise: `git submodule update --init vendor/microsandbox` (slower; clones afresh).

Build:

```sh
# patched agent-vm (adds AGENT_VM_ALLOW_LOCAL_EGRESS escape hatch)
cd <repo-root>
cargo build --release -p agent-vm

# shim + dispatcher + bridge
cd claude-vm-shim
cargo build --release
./install.sh                       # installs everything to /opt/claude-vm-shim/
# install.sh creates ~/.local/bin/claude-shimmed but doesn't copy the
# patched agent-vm — do that manually so the dispatcher's binary-search
# picks it up:
cp ../target/release/agent-vm /opt/claude-vm-shim/bin/agent-vm
```

After install:

```
/opt/claude-vm-shim/
├── bin/
│   ├── agent-vm                  # patched (AGENT_VM_ALLOW_LOCAL_EGRESS support)
│   ├── claude-shimmed            # the wrapper exposed at ~/.local/bin/
│   ├── claude-vm-bridge          # guest-side relay (staged into each VM)
│   └── claude-vm-dispatcher      # host-side per-session entry
└── lib/
    └── libclaude_vm_shim.so      # LD_PRELOAD library
```

## Using it

```sh
# Sanity check that the wrapper finds the real claude:
CLAUDE_VM_SHIM_LOG=/tmp/cvs.log claude-shimmed --version

# Trigger an actual session — each --bg-spare will dispatch into a fresh VM:
claude-shimmed --bg --print "echo hi"

# Watch dispatcher activity:
tail -F /tmp/claude-vm-dispatcher.log
```

For each session you should see lines like:

```
[dispatcher pid …] bound UDS at /tmp/cc-daemon-0/…/X.claim.sock
[dispatcher pid …] bound TCP at 0.0.0.0:NNNN; guest target = 172.x.y.z:NNNN
[dispatcher pid …] daemon UDS connect accepted
[dispatcher pid …] guest TCP connect accepted from 172.x.y.z:…
[dispatcher pid …] bidi-relay halted
[dispatcher pid …] VM session finished cleanly
```

`/tmp/claude-vm-dispatcher.vm-<pid>.log` captures the agent-vm child's
stderr for each session.

## Env knobs

Read by the wrapper / dispatcher / bridge:

| Env var                          | Effect |
|----------------------------------|--------|
| `CLAUDE_VM_SHIM_LOG`             | Path; shim + dispatcher append logs here (in addition to the fixed `/tmp/claude-vm-dispatcher.log` sink). |
| `CLAUDE_VM_SHIM_PASSTHROUGH=1`   | Skip the VM dispatch; exec a host claude verbatim. Useful for A/B tests. |
| `CLAUDE_VM_SHIM_DISPATCHER`      | Override the dispatcher path the shim invokes. Set by the wrapper script. |
| `CLAUDE_VM_SHIM_SO`              | Path to `libclaude_vm_shim.so` (used by the wrapper). |
| `CLAUDE_VM_SHIM_REAL_CLAUDE`     | Path to the real claude binary (used by the wrapper). |
| `CLAUDE_VM_SHIM_AGENT_VM_BIN`    | Override the `agent-vm` binary path the dispatcher uses. |
| `CLAUDE_VM_SHIM_BRIDGE_DIR`      | Override the dir containing `claude-vm-bridge` (defaults to `/opt/claude-vm-shim/bin`). |
| `CLAUDE_VM_SHIM_WORKDIR_ROOT`    | Override the per-session VM workdir root (defaults to `/tmp/claude-vm-shim-work`). |
| `CLAUDE_VM_SHIM_HOST_IP`         | Override the host IP the guest bridge connects to. Auto-detected from `/proc/net/fib_trie`. |
| `CLAUDE_VM_SHIM_MSB_PATH`        | `MSB_PATH` forwarded to the agent-vm child. Defaults to the npm-bundled `msb`. |
| `CLAUDE_VM_SHIM_DUMP_BYTES`      | Path prefix; if set, the dispatcher appends raw relay traffic to `<prefix>.<dir>.pid<pid>` (and `<prefix>.translated.pid<pid>` for the rewritten claim frame). For protocol debugging. |
| `AGENT_VM_ALLOW_LOCAL_EGRESS=1`  | Read by patched `agent-vm`; flips the network policy to `non_local()`. The dispatcher sets this on each spawn. |

## How interception works (LD_PRELOAD with Bun)

The Claude Code binary is a Bun-compiled standalone. Bun's spawn primitives
call libc's `execve`/`posix_spawn` through normal dynamic linkage (PLT/GOT),
not via `dlopen("libc.so.6")` + `dlsym(handle, "execve")`. That means
`LD_PRELOAD` overrides do take precedence. Confirmed empirically — see
`tail -F /tmp/cvs.log` after running `claude --bg`.

The shim matches on `argv[1] == "--bg-spare"` rather than on argv[0]'s path,
because the daemon spawns with `pinToCurrentBinary: true` and the path is
`/root/.local/share/claude/versions/<v>` (it can change on upgrade).

The dispatcher unsets `CLAUDE_VM_SHIM_DISPATCHER` before any inner exec, so
the shim short-circuits and never recurses.

## Bg-spare claim protocol

The wire format on the claim socket (verified by capturing real traffic):

```
daemon                                       bg-spare
  │ connect(claim_sock)                            │
  │ write(JSON.stringify({cwd, env, argv, sessionId}) + "\n")
  │ shutdown(WR)                                   │
  │                                                │ adopt: chdir(cwd),
  │                                                │ set env, run session
  │ read(...) ← (typically EOF; some sessions
  │             reply before close)                │
```

The dispatcher reads the claim frame on the host side, applies these rewrites
before forwarding to TCP:

- **`cwd` and `env.PWD`** mapped from host path → in-VM path. When the host
  project is under tmpfs (`/tmp/…`, `/run/…`, `/dev/shm/…`), agent-vm mounts
  it at `/workspace`, so those paths map there; otherwise the path is mirrored.
- **`env`** filtered to drop:
  - shim plumbing (`LD_PRELOAD`, `CLAUDE_VM_SHIM_*`),
  - outer-VM leakage (`KRUN_*`, `MSB_*`),
  - host-only paths the in-VM claude can't reach (`CLAUDE_BG_RENDEZVOUS_SOCK`),
  - Bun's `_` (invocation wrapper, host-specific).

After translation, the daemon's "claim" is accepted by the in-VM claude and
the daemon's session log records `bg settled <id> (done)`.

## What works

- ✅ Per-session VM boot (one fresh agent-vm per `--bg-spare`).
- ✅ Daemon claims spare → adoption succeeds.
- ✅ In-VM claude talks to api.anthropic.com (auth via `/agent-vm-state`).
- ✅ Session output reaches `claude logs <id>` (via the inherited PTY).
- ✅ `claude stop <id>` shuts down the VM session.
- ✅ Multiple parallel spares pre-seeded by the daemon, each its own VM.

## Known gaps

- **Rendezvous-sock relay.** `CLAUDE_BG_RENDEZVOUS_SOCK` is currently stripped
  from the claim frame. That socket is how the daemon streams *state*
  (heartbeats, agent-mode transitions, repaint coordination after resize, and
  the `done` signal) — but **not** the output stream, which flows over the
  PTY socket and works today. Sessions function without it; we'd want it
  added for clean shutdown signaling and to avoid the daemon's
  "heartbeat-missing" timeout path. Same architecture as the claim-sock
  relay (host UDS bind + TCP listener + guest bridge), just long-lived.
- **PTY size from in-VM.** The PTY is sized by the host's `--bg-pty-host`
  args (`200x50` defaults). Resize messages over the rv channel can't reach
  the in-VM claude, so terminal resize during a session won't propagate.
- **Project paths outside tmpfs.** The path translator currently only handles
  the `/tmp/`, `/run/`, `/dev/shm/` → `/workspace` case. For projects under
  `/home/<user>/`, agent-vm mirrors the host path inside the VM and no
  translation is needed; but we don't verify the patched mount actually
  materialized that path inside the guest.
- **OAuth refresh inside the VM.** The in-VM claude reads OAuth tokens from
  `/agent-vm-state/claude/.credentials.json` via the state mount. Refresh
  works through the existing intercept-hook + secrets layer. Not yet tested
  with an expired token.
- **PTY / interactive stdio.** The dispatcher inherits the PTY slave fds from
  the outer `--bg-pty-host`, but `agent-vm shell -- bash -c …` runs
  non-interactive ("no TTY; streaming output"). The bg-spare protocol may
  need a real TTY. agent-vm has interactive mode but needs `-t` (or the
  `--tmux` flag).
- **Auth credentials in the VM.** Each VM boot would normally inherit the
  user's claude.ai OAuth credentials via the state mount. Hasn't been
  exercised end-to-end with a real session.
- **Spare-VM cost.** The daemon pre-seeds bg-spare workers ahead of any
  session request, so every claude launch boots a VM that may never serve a
  session. Acceptable for the prototype; in production you'd want a lazy
  boot path or cap concurrent spares.
- **Resource budget.** `--no-git` is hard-coded to free a virtio IRQ slot
  (the IRQ pool is tight under nested virt). A bind-mount-based bridge
  delivery would consume that slot, so we copy the binary into the per-session
  workdir instead.
- **Single platform.** Hard-coded to x86_64 Linux. The dispatcher's
  agent-vm-binary candidate list and the bridge's gateway-detection logic
  would need to grow for other platforms.

## Files touched outside this directory

- `crates/agent-vm/src/run.rs` — adds the `AGENT_VM_ALLOW_LOCAL_EGRESS`
  policy escape hatch (≈14 lines). The default behavior is unchanged.
