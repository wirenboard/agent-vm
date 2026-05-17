# agent-vm — ARCHITECTURE

How the rewrite is put together and *why*. Reading this top-to-bottom should
tell you what every nontrivial design choice in the codebase exists for.
Updated after each phase lands. Section per phase; subsection per major
decision.

## Phase 0 — Scaffolding

### Repository layout

```
microsandbox-rewrite/
├── PLAN.md                     # phased roadmap
├── ARCHITECTURE.md             # this file
├── Cargo.toml                  # workspace
├── crates/
│   └── agent-vm/
│       ├── Cargo.toml
│       └── src/main.rs         # hello-world sandbox boot
├── vendor/
│   └── microsandbox/           # git submodule, wirenboard/microsandbox
└── .gitmodules
```

### Why a Cargo workspace from day one

The binary is small today but we already know we'll need at least one
internal crate per concern (creds, image, session). A workspace lets us add
those without restructuring later, and keeps `vendor/microsandbox` out of
our crate's manifest noise.

### Why a git submodule for microsandbox (vs. crates.io, vs. path dep)

- **Phase 3 requires extending microsandbox.** The new `SecretValue::File`
  variant lives in `microsandbox-network`. A path dep against a sibling
  checkout works for one developer but not for CI or contributors. A submodule
  pinned to a branch on our fork (`wirenboard/microsandbox`) makes the
  checkout self-contained and the upstream diff reviewable.
- **`[patch]` against crates.io** also works, but it duplicates the source-of-
  truth pointer (Cargo.lock + patch table) and hides the fact that we are
  shipping a fork. Submodule is more explicit.

### Why depend on the path under `vendor/microsandbox` even before we fork

Phase 0 doesn't change microsandbox, but we point at the submodule path so
the build wiring we set up here is the same wiring Phase 3 uses. Avoids a
mid-rewrite refactor of `Cargo.toml`.

### Why `Sandbox::builder("hello").image("alpine")` for the smoke test

Smallest possible exercise of the SDK that proves we can talk to the runtime.
Alpine is in the microsandbox examples, downloads quickly, and exits cleanly.
No need to involve our own image (that's Phase 1).

### Phase 0 runtime validation

`cargo run -p agent-vm` was exercised end-to-end on a Linux KVM host:

- One-time setup required outside the source tree: `apt install libcap-ng-dev`
  (link-time dep pulled in transitively by `msb_krun`'s `capng` crate), and
  user membership in the `kvm` group so `/dev/kvm` is openable. Both are host
  prerequisites and don't belong in the repo.
- microsandbox's build script downloads its prebuilt runtime artifacts the
  first time `cargo check` runs against the workspace
  (`microsandbox@0.4.6: downloading microsandbox runtime dependencies`).
  Nothing in our crate has to opt into this; the `prebuilt` feature is on by
  default in `microsandbox-runtime`.
- Wall-clock for the full boot + `echo` + teardown with the alpine image
  already in cache: **2.7s** on a release build. Cold first run includes the
  OCI pull on top.

This is the latest point we can confirm we're talking to a real runtime before
adding our own scaffolding; pinning the validation here means a Phase 1 image
regression won't masquerade as an SDK-integration regression.

## Phase 1 — Base OCI image

### Layout

```
images/
├── Dockerfile        # Debian 13 slim + agents
└── build.sh          # ensures registry, docker build, docker push

crates/agent-vm/src/
├── main.rs           # clap entry; dispatches to subcommands
└── setup.rs          # `agent-vm setup`: invoke build.sh, then verify in microsandbox
```

### Image distribution: local Docker registry vs. alternatives

`RootfsSource` (microsandbox-side) supports three image origins:

1. `Oci(reference)` — pulled from a registry (Docker Hub, GHCR, local, etc.).
2. `Bind(path)` — host directory used as the rootfs directly.
3. `DiskImage(path)` — qcow2/raw/vmdk file.

We pick **(1) with a local `registry:2` container on 127.0.0.1:5000**, exposed
to the sandbox builder as `.image("localhost:5000/agent-vm:latest")
.registry(|r| r.insecure())`. Rationale:

- **Standard OCI semantics.** microsandbox's layer cache, GC, snapshotting,
  and metadata DB all key off OCI references. Going through the registry path
  means we get all of that for free instead of working around it.
- **Same wiring as a future remote registry.** When/if we publish images to
  GHCR, the launcher's `.image(...)` call doesn't change; only the tag does.
- **Bind would require write-through or COW management.** `RootfsSource::Bind`
  hands the host directory to the VM as the rootfs. The microsandbox example
  uses it for a single one-shot sandbox; we'd need an overlay on top to share
  a template across multiple concurrent invocations. The OCI path already
  handles this via the layer cache.
- **Disk-image (qcow2) would mean building rootfs images ourselves.** Doable
  with `debootstrap` + `mkfs.ext4`, but the build steps are less familiar
  than `docker build` and the rebuild loop is slower.

The price is that we now run a Docker daemon and a `registry:2` container on
the host. Acceptable: every dev who needs to *build* the image already needs
Docker, and the registry container is ~30 MB and starts in <1 s. End users who
only pull a prebuilt image won't run the local registry at all (Phase 5+
territory).

### Image content: deliberately minimal

The current Dockerfile installs only what each of the three agents needs to
run plus the dev tools that are universally useful:

- Base: `ca-certificates`, `curl`, `wget`, `git`, `jq`, `bash`,
  `python3`/`pip`, `ripgrep`, `fd-find`.
- Node.js 22 from NodeSource (needed by Claude Code, OpenCode, MCP servers).
- Agents installed via their canonical installer scripts so we track upstream
  release channels: `claude.ai/install.sh`, `opencode.ai/install`, and the
  Codex `install.sh` from GitHub releases.

Explicitly skipped in v1 (per PLAN.md scope cuts): Docker-in-VM, Chromium,
LSP plugins, Chrome DevTools MCP, `mitmproxy` (microsandbox does the
interception in Phase 3, no in-VM proxy needed), `gh`, GitHub Copilot CLI.
Each line we keep is a line that has to keep working through `apt-get update`
churn, so the bar to add anything is "needed by an in-scope agent flow."

Resulting image: ~1.5 GB uncompressed locally, ~350 MB on disk in the
registry (compressed layers). Node.js itself is the biggest contributor.

### Image build is shelled to Bash, not done in Rust

`crates/agent-vm/src/setup.rs::run_build_script` spawns
`bash images/build.sh`. We don't talk to the Docker daemon directly because:

- Docker has a CLI that every developer already knows how to read, run, and
  debug. A Rust caller wrapping the API would only add a layer.
- The build script is the right place for host-shell idioms (volumes,
  port-forwarding the registry, `docker inspect` checks) and stays out of
  the way of the Rust binary's logic.
- Rebuilding the image doesn't require recompiling the binary, and vice
  versa.

The Rust side does own the **verify** step (boot from the freshly pushed
image, run the three `--version` commands), because that step is exactly the
microsandbox SDK call the launcher will make in Phase 2 — exercising it from
`setup` ensures we catch image/SDK-integration regressions before any user
session depends on them.

### `setup --no-verify` and `--image`

Two escape hatches surfaced from the start:

- `--no-verify` lets a developer iterate on the Dockerfile without paying for
  a sandbox boot each loop.
- `--image` / `AGENT_VM_IMAGE_TAG` lets us point at an alternative tag (a
  prebuilt image on GHCR, a developer's experimental tag, etc.) without
  touching `build.sh`. The default stays `localhost:5000/agent-vm:latest` so
  the happy path matches what `build.sh` produces.

## Phase 2 — Launcher MVP

### Layout

```
crates/agent-vm/src/
├── main.rs           # clap entry: setup | claude | codex | opencode | shell
├── setup.rs          # unchanged from Phase 1
├── run.rs            # `agent-vm <agent>`: build sandbox, attach or exec
└── session.rs        # project-dir hash, state dirs, sandbox name
```

### Project-scoped sandbox name + state dir

Each project gets:

- A short hash: first 6 bytes of `SHA256(canonical(cwd))` rendered as 12 hex
  chars (~48 bits — plenty for "no two project dirs on one host collide").
- A state directory at
  `${AGENT_VM_STATE_DIR-${XDG_STATE_HOME-$HOME/.local/state}/agent-vm}/<hash>/`.
- A sandbox name of `agent-vm-<hash>`.

The hash being short enough to fit in `<hostname>` is convenient when
debugging from inside the sandbox (`hostname` shows it). The sandbox name
being deterministic means a second `agent-vm claude` in the same project
*replaces* the first one (`.replace()` on the builder gives it 10 s to exit
gracefully, then SIGKILLs) instead of spawning a parallel VM.

The launcher prints a one-line banner on startup
(`==> agent-vm-<hash> in <cwd> (state: <dir>)`) so users always know which
project a given sandbox is bound to.

### Mounts: one for workspace, one for state, no third

The microsandbox runtime caps virtio devices via libkrun's IRQ pool, and an
OCI rootfs already consumes two slots (EROFS lower + ext4 upper). Adding a
bind mount per agent state directory (claude, codex, opencode) puts us over
the cap — `RegisterBlockDevice(IrqsExhausted)` at boot.

Resolution:

- One bind mount for `cwd → /workspace` (the project).
- One bind mount for `<state-dir> → /agent-vm-state` (everything else).
- The agents' expected paths are wired up *inside* the rootfs by Phase 1's
  `patch` API (rootfs symlinks baked into the upper overlay before VM start):
  - `/root/.claude → /agent-vm-state/claude`
  - `/root/.local/share/opencode → /agent-vm-state/opencode`
  - Codex uses the `CODEX_HOME=/agent-vm-state/codex` env var instead of a
    symlink, because `/root/.codex/packages/...` in the base image contains
    the codex binary itself and a symlink would shadow it.

This keeps us at two virtio bind mounts no matter how many agents we add
later.

### Interactive attach vs. non-TTY exec

`Sandbox::attach()` requires a real controlling TTY: it puts stdin in raw
mode and opens `/dev/tty` for its non-blocking input fd. When stdin isn't a
TTY (pipe, redirect, smoke test under `sg`/`sudo -c`, CI), `attach` returns
ENXIO.

The launcher checks `std::io::stdin().is_terminal()` and branches:

- **TTY** → `attach(cmd, args)` — the agent's TUI gets a full PTY and is
  fully interactive.
- **No TTY** → `exec_with(cmd, |e| e.args(args).cwd("/workspace"))` — runs to
  completion, then forwards collected stdout/stderr and the exit code.

Non-TTY mode loses the live streaming TUI experience but gives the caller a
clean `stdout | other-tool` story. Streaming exec output during run is on
the Phase 5 polish list.

### Credentials: env-var only, deliberately

Phase 2 reads `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` from the host
environment and forwards them via `.env()`. This is the simplest possible
path that exercises everything else end-to-end. Phase 3 replaces this with
microsandbox's secret-substitution API backed by host-rooted token files,
and Phase 4 adds refresh semantics on top.

Concretely, the env-var path is intentionally insufficient for our real use
case (Claude Code's host OAuth, Codex's host ChatGPT auth, OpenCode's
OAuth flows). That gap stays open until Phase 3.

### `PATH` is set explicitly, not inherited

The Phase 1 Dockerfile puts the agent binaries on `PATH` via an `ENV`
directive, but that PATH only takes effect when an interactive shell sources
the image's profile. `attach()` and `exec()` both spawn the command via
`execve` directly, so we re-publish the same PATH on the sandbox builder
(`/root/.local/bin:/root/.claude/local/bin:/root/.opencode/bin:/usr/local/
bin:/usr/bin:/bin`). Otherwise `agent-vm claude` would `ENOENT` immediately.

### Tunables: env-var-driven for now

`AGENT_VM_IMAGE_TAG`, `AGENT_VM_MEMORY_MIB`, `AGENT_VM_CPUS` cover the three
knobs you actually want to change session-to-session. Promoting these to
proper clap flags is deferred to Phase 5 polish — env-var-only keeps the
Phase 2 surface small and means we don't have to design the `--memory 4G`
vs `--memory 4096` ergonomics yet.

### What Phase 2 deliberately doesn't do

- **No live DoD smoke against the Anthropic API.** The Phase 2 DoD in
  PLAN.md calls for `agent-vm claude -p 'say hi'` returning a real Claude
  response, but on this host we only have a Claude OAuth credential (not an
  `ANTHROPIC_API_KEY`), and Phase 2 explicitly does *not* implement OAuth
  plumbing. We verified all of Phase 2's wiring end-to-end via the `shell`
  subcommand (workspace mount, state persistence, env propagation, all three
  agent CLIs resolvable on PATH) and explicitly chose to close the API-call
  gap during Phase 3's host-OAuth work rather than ferry an ephemeral API
  key through this session.
