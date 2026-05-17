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

### Phase 2 — Launcher MVP [done — wiring complete; live API smoke deferred to Phase 3]

- clap-based subcommand parser: `setup | claude | codex | opencode | shell`.
- Project hash + state dir helper (`${XDG_STATE_HOME:-~/.local/state}/agent-vm
  /<hash>/`).
- Mount `cwd` at `/workspace` inside the sandbox.
- Persist `~/.claude` and `~/.local/share/opencode` via rootfs-patched
  symlinks into a single `/agent-vm-state` bind mount; redirect `~/.codex`
  via `CODEX_HOME` (its binary lives under that path, so a symlink would
  shadow it). One bind for project, one for state — total two virtio mounts
  on top of the OCI rootfs, well under libkrun's IRQ cap.
- TTY-conditional dispatch: `attach()` when stdin is a real terminal,
  `exec_with(...)` otherwise (handles pipes, redirects, smoke tests under
  `sg`/`sudo -c`, CI).
- Credentials: env-var only (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`) so the
  launcher is independent of the refresh machinery.

**Done when:** `cd repo && agent-vm claude -p "say hi"` returns a real Claude
response from inside the sandbox.

**Actual outcome:** All wiring verified via `agent-vm shell` (workspace
round-trip, persistence across reboots, agent CLIs resolvable on PATH,
CODEX_HOME redirect, env propagation). The live API smoke was deferred — see
ARCHITECTURE.md "What Phase 2 deliberately doesn't do". Phase 3's host-OAuth
work closes the gap naturally.

### Phase 2.x — Post-MVP polish [done — commits `7608f27`..`d3914b9`]

A series of small fixes landed between Phase 2 and Phase 3, all triggered by
real testing on the user's laptop. Listed here so the next reader knows
they're in already and doesn't redo the work.

- **`RUST_LOG` wiring** (`7608f27`). `tracing-subscriber` initialized in
  `main`, defaults to `warn`. The microsandbox stack is silent otherwise.
- **Auto-recover the local registry container** (`a57ed6d`). `build.sh`'s
  `ensure_registry` was rewritten as a state machine that handles every
  `docker ps` state, polls `/v2/` after start, and recreates from scratch
  if a stale container is running with no port mapping. Plus per-phase
  banners so long waits don't look like a hang.
- **Mirror the host project path inside the guest** (`92ff582`). `cwd` is
  bind-mounted at the same absolute path, so anything the agent emits
  (compiler errors, stack traces, file:line references) is interpretable on
  the host. Paths under tmpfs mount points (`/tmp`, `/run`, `/dev/shm`,
  `/var/run`) fall back to `/workspace` with a warning, because the guest
  tmpfs-mounts them at boot and wipes any patch-created mount point.
- **`AGENT_VM_PROFILE=1`** (`127f6b3`). Prints per-phase wall-time
  (create / run / stop / remove) for the launcher. Confirmed total is
  ~1.5 s, dominated by VM boot (~1.0 s of libkrun kernel boot).
- **Pull progress bar** (`2489168`..`66ed8f3`..`3ffd6b6`..`984680a`).
  Two-phase indicatif renderer: spinner with text during download, real
  byte-weighted bar during materialize. Single line, single spinner, ETA
  based on materialize-only rate (no more "29 minute" → "17 second" jumps).
- **`agent-vm pull` + per-launch update-available banner**
  (`bfab9d3`..`d3914b9`). Pulls are explicit; `agent-vm shell` only does a
  cheap manifest-digest HEAD against the registry and prints a banner when
  the per-platform digest differs from what we last pulled. The "what we
  last pulled" digest is tracked in our own marker file
  (`~/.local/state/agent-vm/pulled-digests/<hash>`), atomically written
  only after a successful pull, so an interrupted pull never leaves the
  microsandbox cache in an empty or stale state.

### Phase 3 — Static host-rooted secrets [next]

The big architectural payoff of moving to microsandbox: real tokens never
enter the VM.

- Branch `vendor/microsandbox` to add a `SecretValue::File(PathBuf)` variant
  alongside the existing `String` value. The TLS-intercept proxy re-reads
  the file on every substitution. Required because microsandbox's
  `SecretEntry.value: String` is captured at sandbox-builder time, with no
  runtime update path.
- Enable TLS interception on the sandbox (`network(|n| n.tls(...))`) —
  microsandbox's secrets are only substituted on TLS-intercepted
  connections to allowed hosts, and the guest needs the sandbox CA in its
  trust store.
- `agent-vm` on startup snapshots host `~/.claude/.credentials.json` and
  `~/.codex/auth.json` into `<state>/tokens/{anthropic,openai}.token`
  files (just the `access_token`, never the refresh token).
- Register file-backed secret entries for `api.anthropic.com`,
  `api.openai.com`, `chatgpt.com`, `platform.claude.com`,
  `auth.openai.com`, etc.
- Cross-instance lockfile under `<state>/tokens/` so two `agent-vm`
  processes don't race when both want to refresh.

**Done when:** inside the guest, `cat /proc/$$/environ | tr '\0' '\n' |
grep -i token` shows only placeholders, while a real Claude (host OAuth)
request succeeds end-to-end and writes a turn to disk.

### Phase 4 — Refresh semantics [pending]

Tokens rotate; long-running sandbox sessions must survive that without
re-attaching. Phase 3 makes the access token swappable; this phase teaches
agent-vm to actually do the swap.

- inotify (Linux) / kqueue (macOS) watcher on host creds files: when host
  Claude/Codex rotates tokens externally (the user runs `claude` on the
  host, etc.), we re-snapshot to the file microsandbox watches.
- Proactive expiry watch: when the access token is < 5 minutes from
  expiry and no host activity has refreshed it, spawn `claude -p "ping"
  --model sonnet` / `codex exec --skip-git-repo-check "Reply OK"` on the
  host so the host-side CLI rotates the file.
- Single-flight per credential; cross-instance lockfile so concurrent
  `agent-vm` instances don't all kick off a refresh at once.

**Done when:** a multi-hour session crosses a token rotation without the
agent seeing an auth error and without manual intervention.

### Phase 5 — Snapshot-based fast launch [pending — perf win]

Surfaced during Phase 2.x profiling and discussed with the user when they
asked why `agent-vm pull` was so slow. Every `agent-vm shell` currently
spends ~1.0 s booting the libkrun kernel + initing the guest agent;
microsandbox has a snapshot/restore API that should drop that to ~100 ms
*and* skip the EROFS materialize step on a re-pull.

- One-shot snapshot generation: `agent-vm setup` (or a new `agent-vm
  snapshot`) boots the image once, lets it reach a quiet steady state,
  takes a snapshot, and stashes it under `<microsandbox-home>/snapshots/`.
- Launcher: if a snapshot exists, use `Sandbox::builder(...).from_snapshot
  (snapshot_path)` instead of `.image(...)`. Falls back to cold boot if
  the snapshot is missing or wrong-digest.
- Re-snapshot whenever `agent-vm pull` lands a new manifest (snapshots are
  pinned to a specific image digest by microsandbox).
- Bake the per-agent symlinks + state mount point into the snapshot so
  the launcher doesn't need to re-apply patches each invocation.

**Done when:** `time agent-vm shell -- -c true` is well under 500 ms in
the steady state.

### Phase 6 — Distribution + polish + docs [pending]

The "ready to share with a teammate" phase.

- **Auto-install of the microsandbox runtime.** The agent-vm binary ships
  on its own; `~/.microsandbox/{bin/msb, lib/libkrunfw.so.5.2.1}` are
  needed but not bundled. Wrap `microsandbox::setup::install()` so first
  run downloads them automatically if missing. Verify version matches the
  prebuilt the binary was built against.
- **CLI flag promotion.** `--memory N`, `--cpus N`, `--image REF`,
  `--no-update-check`. Today these are env-var only (`AGENT_VM_*`).
- **`.agent-vm.runtime.sh` project hook.** Script executed in the guest
  immediately before the agent starts, for project-local setup
  (`npm install`, `docker compose up`, etc.).
- **README rewrite.** Install, prereqs, setup, usage, troubleshooting,
  the registry/marker/snapshot internals at a high level.
- **CI smoke test.** GitHub Actions workflow that builds the image, runs
  `agent-vm setup --no-verify`, and `agent-vm shell -- -c 'echo ok'`.
- **macOS/aarch64 binary.** Cross-compile or native-build on each
  platform microsandbox supports.

**Done when:** README is publishable, CI smoke green on at least linux-amd64,
binary works from a fresh checkout on a host where microsandbox runtime is
not pre-installed.

## Discovered upstream issues

Things we worked around during Phase 2.x that should eventually be filed
or fixed in `wirenboard/microsandbox`:

1. **`PullPolicy::Always` doesn't refresh the cached manifest digest.**
   It re-fetches layer blobs correctly, but `Image::persist`'s fast-path
   detection skips the DB update under the same reference even when the
   per-platform manifest digest changed. We work around it with our own
   marker file rather than `Image::remove` (because remove + re-pull
   opens an empty-cache window).
2. **`LayerDownloadProgress` events are often elided** for fast registries
   (we never see them with localhost). Only `LayerDownloadComplete` fires.
   Not exactly a bug, but undocumented and bit us when we tried to drive a
   download-bytes bar.
3. **libkrun virtio IRQ cap is low** (~6 devices). We're constrained to
   2 bind mounts on top of the OCI overlay's 2-device cost. Bigger fan-out
   needs upstream tuning of the libkrun build.
4. **Manifest media-type assumptions.** Microsandbox stores the
   per-platform manifest digest; a registry HEAD on a tag returns the
   multi-arch index digest by default. Either would be fine to use, but
   the SDK doesn't expose either as "ask the registry what's there now"
   so we end up doing raw HTTP. A `Image::resolve(reference) -> RemoteRef`
   helper would clean this up.

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
