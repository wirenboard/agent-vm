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
