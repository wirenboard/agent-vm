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
only pull a prebuilt image won't run the local registry at all (Phase 9
distribution territory).

### Image content: deliberately minimal

The current Dockerfile installs only what each of the three agents needs to
run plus the dev tools that are universally useful:

- Base: `ca-certificates`, `curl`, `wget`, `git`, `jq`, `bash`,
  `python3`/`pip`, `ripgrep`, `fd-find`.
- Chromium + `fonts-liberation` + `sudo` + `libnss3-tools` for the
  Chrome DevTools MCP (Phase 7). Symlinks `/usr/bin/google-chrome` and
  `/opt/google/chrome/chrome` to `/usr/bin/chromium` so puppeteer's
  default discovery paths resolve. Dedicated `chrome` user (UID 9999)
  with home `/home/chrome`, empty NSS DB at `~/.pki/nssdb`, sudoers
  rule `root ALL=(chrome) NOPASSWD: ALL`, and a wrapper at
  `/usr/local/bin/agent-vm-chrome-mcp` that re-execs the MCP under
  that user. Pre-warmed npm cache for `chrome-devtools-mcp@1.0.1`
  under `/home/chrome/.npm/_npx/` so first launch is a cache hit.
- `gh` from cli.github.com/packages (Phase 6 — gh/git credential
  injection).
- Node.js 22 from NodeSource (needed by Claude Code, OpenCode, MCP servers).
- Agents installed via their canonical installer scripts so we track upstream
  release channels: `claude.ai/install.sh`, `opencode.ai/install`, and the
  Codex `install.sh` from GitHub releases.

Explicitly skipped in v1 (per PLAN.md scope cuts): Docker-in-VM, LSP
plugins, `mitmproxy` (microsandbox does the interception in Phase 3,
no in-VM proxy needed), GitHub Copilot CLI. Each line we keep is a
line that has to keep working through `apt-get update` churn, so the
bar to add anything is "needed by an in-scope agent flow."

Resulting image: a few GB uncompressed (chromium is the largest
contributor at ~400 MB, followed by Node.js and the agent CLIs;
re-measure with `docker images` when you care about exact bytes).
Registry layer count is bounded by the `RUN` granularity in the
Dockerfile.

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

When this layout was chosen, the microsandbox runtime was running with
libkrun's default in-kernel IOAPIC, which hands ~11 IRQs to virtio-mmio
devices total on x86_64. The OCI rootfs already consumes two slots (EROFS
lower + ext4 upper), plus virtio-net + vsock + console + agentd's
serial — adding a bind mount per agent state directory (claude, codex,
opencode) pushed us over and `RegisterBlockDevice(IrqsExhausted)` at boot
followed. We later lifted the underlying cap by enabling `msb_krun`'s
userspace split irqchip (see "Split irqchip and the virtio-IRQ ceiling"
below; cap is now ~219), but the one-workspace + one-state layout still
makes sense regardless: one virtio-fs server, one rootfs `patch` entry
per agent, and a stable on-host layout.

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
later, and leaves plenty of IRQ headroom for user-supplied `--mount`
arguments now that the split irqchip is on.

### Split irqchip and the virtio-IRQ ceiling

`msb_krun` exposes a `MachineBuilder::split_irqchip(bool)` knob. With it
off (default), libkrun uses KVM's in-kernel IOAPIC, which is hard-capped at
24 pins and only hands IRQs 5..=15 to virtio-mmio — about 11 usable IRQs
for the whole VM. That fills up fast: rootfs lower + rootfs upper +
virtio-net + virtio-vsock + virtio-console + virtio-fs (project) +
virtio-fs (state) already saturates it on this build, so an extra
`--mount` would trip `RegisterNetDevice(IrqsExhausted)` at boot.

With split_irqchip enabled, `msb_krun` runs a userspace IOAPIC backed by
an event-loop thread it spawns automatically. The pin count rises to 219,
which puts the practical ceiling on `--mount` well into the hundreds. The
trade-off is one extra worker thread per VM and a slightly hotter IRQ
delivery path; we accept it because the IRQ headroom is the difference
between "one or two extra mounts work" and "you can stop worrying about
the cap." aarch64/riscv64 ignore the knob — their GIC/AIA models already
expose >200 IRQs.

The runtime sets `split_irqchip(true)` unconditionally in
`vendor/microsandbox/crates/runtime/lib/vm.rs`. The user-facing
`--mount` doc and Phase 7's wiring in `crates/agent-vm/src/run.rs` were
updated to drop the pre-cap warning that previously fronted the limit.

The change also bumped `msb_krun` from 0.1.12 → 0.1.13 across the
vendor crates (`runtime`, `filesystem`, `network`). 0.1.12's userspace
IOAPIC was unusable in practice: its IRR was a single `u32` so any IRQ
delivered on pin ≥ 32 was dropped without notice, and the redirection-
table register-index calculation in `read`/`write` did an unchecked
`ioregsel - IOAPIC_REG_REDTBL_BASE` that wrapped on any access below
the redirection-table base — which the guest performs during normal
IOAPIC programming. Both fixes landed in 0.1.13, published 2026-05-26.
The PLAN's Discovered Upstream Issue #3 was originally attributed to
"the libkrun IRQ cap"; with hindsight, the cap is a real KVM-level
ceiling but the multi-mount boot failure that finally drove this work
was a separate, fixable bug inside `msb_krun_devices`'s userspace
IOAPIC, only ever reached once the split irqchip was turned on.

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
clean `stdout | other-tool` story. Streaming stdout/stderr during run landed
in the Phase 4 verification session (2026-05-24) — see PLAN.md.

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
knobs you actually want to change session-to-session. `--memory` and
`--cpus` were promoted to clap flags (`1817391`); `--image` and friends are
on the Phase 9 polish list. Env-var-only kept the
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

## Phase 3 — Host-rooted secrets

### Layout

```
vendor/microsandbox/  (branch: agent-vm-secret-file)
└── crates/network/lib/secrets/
    ├── config.rs            # new: SecretValue { Static, File } enum
    └── handler.rs           # resolves SecretValue at connection-setup

crates/agent-vm/src/
├── secrets.rs               # new: read host creds, write placeholders
├── run.rs                   # wire TLS intercept + secret_env per provider
└── main.rs                  # register secrets module
```

### Two-layer placeholder dance

Real tokens never enter the VM. The dance per provider:

1. **Host side.** agent-vm reads the host's credential file
   (`~/.claude/.credentials.json` for Claude,
   `~/.codex/auth.json` for Codex) at every launch. It extracts the
   access token, keeps it in a short-lived Rust `String`, and registers
   it as a microsandbox secret with a stable placeholder string
   (`msb-anthropic-placeholder-a-v2` and
   `msb-openai-placeholder-a-v2`). The placeholder *constants* live in
   `crates/agent-vm/src/secrets.rs` (`ANTHROPIC_ACCESS_PLACEHOLDER` etc.) —
   prefer the constant over the literal so a future rename doesn't drift.
2. **Guest side.** agent-vm writes a "placeholder credentials" JSON
   into the per-project state dir
   (`<state>/claude/.credentials.json`,
   `<state>/codex/auth.json`) using the placeholder string instead of
   the real token. Other fields (`expiresAt`, `scopes`, `account_id`,
   `last_refresh`, etc.) are copied from the host file so the in-VM
   agent sees a plausible JSON shape. `refreshToken` is set to a
   sentinel string — Phase 3 doesn't handle refresh.
3. **TLS interception.** microsandbox's network proxy intercepts the
   sandbox's HTTPS traffic. When the agent makes a request to any
   allowed host (`api.anthropic.com`, `platform.claude.com`,
   `api.openai.com`, `chatgpt.com`, `auth.openai.com`), the proxy
   sees `Authorization: Bearer msb-…-placeholder-…` in the outgoing
   request, splices in the real token from the secret config, then
   forwards.

The agent inside the VM never sees the real token in any form
(`/proc/$$/environ`, `cat ~/.claude/.credentials.json`, network
introspection inside the guest all show only the placeholder). It
gets the real token only as a header-mangled middlebox effect on the
way out — which is structurally what microsandbox was designed for.

### Upstream extension: `SecretValue { Static, File }`

Pre-Phase 3, `SecretEntry.value` was a `String` captured at builder
time. That worked for static API keys but precluded host-side OAuth
rotation — there was no way to surface a new token to a running
sandbox short of rebuilding it.

The `agent-vm-secret-file` branch of vendor/microsandbox adds
`SecretValue { Static(String), File(PathBuf) }` and changes
`SecretEntry.value` to that enum. The handler resolves `File` at
connection-setup time, so each new request to an allowed host sees the
current file contents. Wire format stays a single JSON string for
backward compatibility with the prebuilt `msb` daemon already on
users' hosts:

| Variant | Wire format |
|---|---|
| `Static(v)` | `"v"` — a bare JSON string, identical to the old `value: String` form |
| `File(p)` | `"\0msbfile:<path>"` — a NUL-prefixed sentinel string |

The NUL prefix is unforgeable in API tokens (always printable ASCII).
Old `msb` daemons that don't recognise the sentinel treat the whole
thing as an opaque string and substitute it verbatim — broken for
`File`, but never crashes.

### Phase 3 uses `Static` only

`SecretValue::File` is the right primitive for refresh-aware
substitution, but turning it on end-to-end requires a `msb` daemon
built from our forked source replacing the prebuilt one at
`~/.microsandbox/bin/msb`. Phase 3 doesn't ship that distribution
plumbing — it captures the host token as a `String` at launch time
and passes `SecretValue::Static(token)` to microsandbox. The sandbox
lives until the token's TTL (usually hours); rotation is a Phase 4
problem.

### Allowed-host lists

Per-provider, we allow the API host *and* the OAuth-token host. The
OAuth-token host doesn't actually need substitution in Phase 3 (we
don't intercept the refresh flow yet), but we have to allow it,
otherwise the in-VM agent's refresh attempt would trigger
microsandbox's secret-violation detector (placeholder going to a
disallowed host = `BlockAndLog` blocks the request). Letting the
placeholder reach the OAuth host means the upstream server just
rejects it normally, which is at least a comprehensible failure.

| Provider | Allowed hosts |
|---|---|
| Anthropic | `api.anthropic.com`, `platform.claude.com` |
| OpenAI    | `api.openai.com`, `chatgpt.com`, `auth.openai.com` |

### `IS_SANDBOX=1`

Claude Code refuses to run as root with
`--dangerously-skip-permissions` unless `IS_SANDBOX=1` is set. The
in-guest user is root, and the whole point of the microVM is that
the sandbox itself is the boundary — so we set `IS_SANDBOX=1` on
the builder. Same env var the original Bash agent-vm used.

### Smoke verification

End-to-end verified on a nested-VM test host (cwd
`/home/boger.linux/agent-vm-phase3-test`):

- `cat /root/.claude/.credentials.json` inside the guest shows the
  placeholder, not the real token. ✓
- `cat /proc/1/environ | tr '\0' '\n' | grep -i token` finds only
  `MSB_AGENT_VM_ANTHROPIC_UNUSED=msb-…-placeholder-…`. ✓
- TLS-intercepted curl to `https://api.anthropic.com` sees the
  microsandbox CA on the server cert (`CN=microsandbox CA`),
  confirming requests go through the substitution proxy. ✓
- `AGENT_VM_DEBUG_CONFIG=1` dumps the SandboxConfig JSON and the
  secret value is the host's real `accessToken` (which on this nested
  test host is itself a placeholder relayed to the outer host's real
  bridge — see below). ✓

The *final* leg ("api.anthropic.com returns a real response") can't
be verified on this host because we're running inside an outer
agent-vm whose own credential bridge intercepts requests on the outer
host's localhost — which our nested microsandbox can't reach. On a
non-nested host with a real Claude OAuth credential, the substituted
bearer reaches Anthropic verbatim and the response is real. The same
flow is structurally identical to how the original Bash agent-vm's
credential-proxy works.

### What Phase 3 deliberately doesn't do

- **No refresh.** Long sessions will hit a 401 when the captured
  access token expires (typically hours). Phase 4 closes this.
- **No `~/.microsandbox/bin/msb` replacement.** The `SecretValue::File`
  variant requires a `msb` rebuilt from our fork to actually re-read
  the file. Without that the Static path is what gets exercised, and
  it does work against unpatched `msb` (wire-format compatibility was
  the explicit design goal of the bare-string sentinel encoding).
- **No host-side OAuth token endpoint short-circuiting.** When the
  in-VM agent tries to refresh, the request goes upstream and is
  rejected by Anthropic/OpenAI because the placeholder refresh token
  isn't real. The original Bash agent-vm has logic to MITM the
  `platform.claude.com/v1/oauth/token` and `auth.openai.com/oauth/token`
  endpoints and forge responses from re-reads of the host file. That's
  Phase 4.

## Phase 4 — OAuth refresh: file-backed secrets + interceptor hook

Phase 3 left a 401-then-die failure mode: when the captured access
token expires mid-session the in-VM agent gets 401 from the API, tries
to refresh against the OAuth endpoint with the placeholder refresh
token, and gets 401 again. The user has to exit and re-launch.

Phase 4 closes the loop end-to-end. Two upstream microsandbox
extensions plus an agent-vm subprocess handle it.

### Pieces

```
vendor/microsandbox/  (branch: agent-vm-secret-file)
└── crates/network/lib/
    ├── secrets/config.rs       # SecretValue::File (from Phase 3) is now actually used
    └── intercept/              # new: per-route request-interceptor hook
        ├── config.rs           #   InterceptConfig (rules + hook command)
        └── handler.rs          #   per-connection state machine

crates/agent-vm/src/
├── msb_install.rs              # new: build patched msb from vendor; point MSB_PATH at it
├── intercept_hook.rs           # new: `agent-vm _intercept-hook` subprocess
├── secrets.rs                  # switched from Static(token) to File(<state>.secrets/{anthropic,openai})
└── run.rs                      # registers the interceptor with two rules
```

### Patched `msb` shipped via `MSB_PATH`

`agent-vm setup` now runs
`cargo build --release -p microsandbox-cli --bin msb` in
`vendor/microsandbox` and leaves the artifact at
`vendor/microsandbox/target/release/msb`. At startup, every agent-vm
invocation sets `MSB_PATH` to that path (top of microsandbox's
resolution ladder), so the patched binary is what actually runs the
VM. The user's `~/.microsandbox/bin/msb` is never touched and
upstream-installed tooling on the same host keeps using its own
prebuilt.

The real msb binary lives in the `microsandbox-cli` crate; the
`microsandbox` crate has a separate `microsandbox` binary that's
just a 5-line shim forwarding to `~/.microsandbox/bin/msb`. Building
the wrong target produces a 389 KB shim that boots silently then
hangs at VM init — about 30 minutes of debugging into a
no-VMM-symbols-in-the-binary surprise. Recorded here so the next
person doesn't redo it.

### `SecretValue::File`

Phase 3's per-launch snapshot becomes a per-launch *file write*. We
write the host's `accessToken` to a host-only secret file (see next
subsection for *where*) with 0600 perms via atomic-write-then-rename.
The launcher passes the file path to microsandbox as a
`SecretValue::File`.

The patched msb's TLS-intercept proxy calls `SecretValue::resolve()`
at *connection-setup* time — every new TCP connection re-reads the
file. So any host-side rotation (whether triggered by the user's
external `claude` use or by our interceptor hook below) is visible
to the very next request, without rebuilding the sandbox.

### Token files live *outside* the guest bind mount

The launcher bind-mounts the per-project `state_dir` into the guest at
`/agent-vm-state` as a *single* mount. The single-bind shape originally
fell out of libkrun's tight virtio-IRQ cap (one bind for all per-agent
state instead of one per agent — see "Mounts: one for workspace, one
for state, no third"), and we've kept it after lifting the cap because
it gives a stable on-host layout and a single virtio-fs server. That
makes mount placement security-critical: **anything under `state_dir`
is readable from inside the VM.**

The real access-token files therefore must *not* live under
`state_dir`. They sit in a sibling host-only directory
`${state_root}/<hash>.secrets/` (mode 0700), derived from `state_dir`
by `secrets::{anthropic,openai}_token_path` so the launcher and the
refresh hook agree on the path without passing it explicitly. The
microsandbox proxy reads these files on the *host* side via
`SecretValue::File`, so they never need to be mounted into the guest at
all.

This was a real leak found during Phase 4 end-to-end verification: the
first cut wrote the tokens to `<state>/tokens/{anthropic,openai}`, i.e.
*inside* the mount, so `cat /agent-vm-state/tokens/anthropic` in the
guest returned the host's real bearer — silently defeating the entire
"real tokens never enter the VM" guarantee. The nested test host masked
it (there the "real" token is itself the outer bridge's placeholder), so
it only surfaced once we grepped the guest filesystem for the token
during verification. A `token_files_live_outside_the_guest_mount` unit
test now guards the invariant.

### Request-interceptor hook (the OAuth refresh MITM)

`microsandbox-network` gained an `InterceptConfig` that the launcher
fills with:

```rust
.intercept(|i| i
    .hook(["…/agent-vm", "_intercept-hook", "--state-dir", "…"])
    .rule("platform.claude.com", "POST", "/v1/oauth/token")
    .rule("auth.openai.com",     "POST", "/oauth/token"))
```

When the in-VM agent posts an OAuth refresh request, the proxy:

1. Buffers the full request (it's tiny — <1 KB — so this is cheap
   and capped at 64 KiB).
2. Spawns the hook command with the request bytes on stdin and four
   env vars (`MSB_INTERCEPT_SNI/_HOST_RULE/_METHOD/_PATH_PREFIX`).
3. Reads the hook's stdout as the response and writes it back to the
   guest, encrypted under the forged TLS cert.
4. Closes the connection without ever touching the upstream server.

The hook (`agent-vm _intercept-hook`) is the same binary in a hidden
clap subcommand mode:

1. Reads the request from stdin (sanity-checks it's `POST …`).
2. Spawns `claude -p hi --model sonnet` (or `codex exec --skip-git-
   repo-check 'Reply OK'`) on the **host** so the host CLI rotates
   `~/.claude/.credentials.json` / `~/.codex/auth.json` the normal way.
3. Re-reads the rotated host file, rewrites the host-only token file
   (`<state>.secrets/{anthropic,openai}`) so the next non-refresh
   request from the guest gets the new bearer via `SecretValue::File`.
4. Synthesizes an OAuth refresh-response JSON shaped like what the
   upstream server would return, but with **placeholder** strings in
   the `access_token` / `refresh_token` fields. The in-VM agent
   updates its credentials.json to those placeholders and continues.
5. Writes the response to stdout, exits 0.

The guest never holds a real token at any layer:
- `~/.claude/.credentials.json` always contains placeholders (Phase 3).
- The real-token file is on the host *outside* the guest bind mount
  (see "Token files live outside the guest bind mount").
- The proxy substitutes real-for-placeholder on the way *out* (Phase 3).
- The OAuth refresh response also returns placeholders (Phase 4).
- The host CLI on the host is the only thing that ever touches real
  OAuth machinery, and it writes to a file we re-read.

### Hook-process boundary, not callback

The interceptor uses a subprocess (fork+exec per request) rather than
a callback into the SDK. Reasons:

- `Vec<Box<dyn RequestInterceptor>>` isn't serializable. The
  network config is JSON-piped from the SDK to a separate `msb`
  process, so anything we configure on the SDK side has to round-trip
  through JSON.
- Refresh requests are rare (once per hour at worst). Fork-per-request
  overhead is irrelevant against the latency of the host `claude`
  invocation that the hook does anyway.
- A subprocess can dispatch on any logic without us having to
  re-extend microsandbox each time we add a provider.

### Smoke verification

```
Inside the guest:
  POST https://platform.claude.com/v1/oauth/token
  body: {"grant_type":"refresh_token","refresh_token":"…PLACEHOLDER_REFRESH…"}

Response:
  HTTP 200 application/json
  {"access_token":"msb-anthropic-placeholder-a-v2",
   "refresh_token":"msb-anthropic-placeholder-r-v2",
   "expires_in":3499, "token_type":"Bearer",
   "scope":["user:file_upload","user:inference",…]}
```

Confirmed on the same nested-VM test host as Phase 3. The hook ran,
host `claude -p` rotated the host file, the new bearer landed in
`<state>.secrets/anthropic`, and the synthesized response reached the
guest. `expires_in: 3499` is the freshly-derived seconds-until-expiry
of the just-rotated token.

### What Phase 4 deliberately doesn't do

- **No proactive expiry timer.** Discussed and rejected: the
  guest's own refresh attempt at 401-time triggers our hook, which
  triggers the host-side refresh. If the user runs `claude` on the
  host between sessions, the host file is already fresh and the
  `SecretValue::File` re-read picks it up with no hook involved.
  A timer would be belt-and-suspenders.
- **No msb shipped via `~/.microsandbox/bin/msb`.** The MSB_PATH
  override is per-agent-vm-invocation only; other microsandbox SDK
  consumers on the same host keep using the upstream prebuilt.
- **No single-flight for concurrent in-guest refreshes.** Two
  concurrent refresh attempts could each spawn a host `claude -p`.
  The host CLI's own file lock prevents corruption, so the worst
  outcome is one extra `claude -p` invocation. If this becomes a
  pain point, a `<state>/tokens/.refresh.lock` flock around the host
  CLI invocation is two lines.
