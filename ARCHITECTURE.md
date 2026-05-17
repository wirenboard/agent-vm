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
