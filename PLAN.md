# agent-vm — PLAN

Roadmap for the Rust + [microsandbox](https://github.com/wirenboard/microsandbox)
rewrite of `agent-vm`. The old phase-by-phase roadmap (Phases 0–9) has been
retired now that the rewrite is feature-usable; per-phase history lives in
`git log` and the design rationale in `ARCHITECTURE.md`. This file tracks only
**what is left to do** to fully match — and then beat — the original Bash
`agent-vm` that still lives on `main`.

## Where the rewrite stands today

Working and verified for daily use:

- **Agents:** `claude`, `codex`, `opencode`, `shell` (per-project microVM,
  ~1.5 s launch, project bind-mounted at its host path).
- **Host-rooted secrets:** real Claude/Codex/OpenCode tokens never enter the
  VM; the microsandbox TLS-intercept proxy substitutes a placeholder for the
  real bearer on the way out. Tokens live host-side outside the guest mount.
- **OAuth refresh MITM** for Claude + Codex (file-backed `SecretValue::File`
  + `_intercept-hook`), so an externally-rotated host token is picked up on the
  next request without a relaunch.
- **gh / git** auth reused from the host with a **per-launch GitHub repo
  allow-list** enforced at the proxy (off-list push → clean 403).
- **Security snapshot** of the three credential files (SHA-256 at launch,
  re-checked on exit).
- **DX:** `--mount`, `clipboard get/put`, `agent-vm-ccusage`, Chrome DevTools
  MCP (chromium as a dedicated user with the MITM CA trusted in its NSS DB).
- **Image + distribution:** `setup` (Docker build + boot-verify), `pull` +
  per-launch update banner, image-API-version lock, bundled patched `msb` +
  libkrunfw, auto-install of the runtime.
- **Network egress** flags `--publish` / `--auto-publish` / `--allow-egress` /
  `--allow-lan` / `--allow-host` — this **already exceeds** the original, which
  had no per-launch egress controls.

Both the original and the rewrite are **fresh-VM-per-launch**; the rewrite is
*not* missing any persistent-VM lifecycle the original had (see C1 — that's a
new capability, not a regression).

## A. In-scope work to finish

These are within the agreed v1 scope and either unverified or incomplete.
(A5 onboarding config and A6 `.agent-vm.runtime.sh` hook were on this list but
are **already implemented** — `secrets.rs` force-sets `hasCompletedOnboarding`
/ `hasCompletedProjectOnboarding` / per-folder trust, and `run.rs:891-962`
sources the project hook before exec — so they're dropped, not pending.)

- **A1 — Real mid-session token rotation (untested).** The substitution +
  refresh-hook infrastructure exists but no run has crossed a real
  token-expiry boundary end-to-end (Claude ~hours, Codex/ChatGPT ~24 h). Drive
  a long session through at least one rotation without a re-attach. *(was the
  last open item of the old Phase 4.)* Effort: M (mostly a long live session).
- **A2 — Refresh single-flight.** Confirmed absent — `intercept_hook.rs` has no
  flock, so two racing in-guest refreshes each spawn a host-side `claude -p` /
  `codex exec`. Add a `<state>.secrets/.refresh.lock` flock. *(old Phase 4
  "Design" item, never implemented.)* Effort: S.
- **A3 — Project-integrity security snapshot.** Confirmed gap: the rewrite's
  `snapshot_host_creds` / `verify_snapshot` (`secrets.rs:529,542`) fingerprint
  **only the three credential files**. The original's
  `_claude_vm_security_snapshot` / `_check` (claude-vm.sh:1560) also
  fingerprints the **project repo** — `.git/config`, `.git/hooks/*`,
  `CLAUDE.md`, `Makefile`, the runtime hook — to catch an off-rails agent
  tampering with git hooks or build files. Extend the snapshot to cover those
  and warn on unexpected change. Effort: M.
- **A4 — Push-access probe.** Confirmed gap: no `git push --dry-run` anywhere
  in the rewrite; the allow-list is built from static `git remote -v` parsing.
  The original probes with `git push --dry-run`
  (`_claude_vm_check_push_access`:1413) to confirm real push rights before
  trusting a remote. Decide whether to add the live probe (it costs a network
  round-trip per launch). Effort: S.

## B. Distribution / release (old Phase 9 leftovers)

- **B1 — CI smoke workflow.** GitHub Actions: build the image, run
  `agent-vm setup --no-verify`, then `agent-vm shell -- -c 'echo ok'`. Green
  on at least linux-amd64.
- **B2 — Cross-arch binaries.** macOS / aarch64 builds + per-platform npm
  packaging (the package currently bundles a linux-x86_64 binary).
- **B3 — IPv6 DNS workaround → upstream fix.** Replace the per-launch
  `sed`-out-the-v6-nameserver hack with either a real fix to the v6 gateway
  DNS path in microsandbox or a `network.dns(disable_ipv6)` knob (upstream
  issue #5).

## C. Improvements beyond `main` (optional, product call)

- **C1 — Detached / persistent-VM fast launch.** Neither original nor rewrite
  has this. Boot once per project, attach per invocation: ~1.5 s → ~10–50 ms.
  Pulls in lifecycle subcommands (`ps` / `stop` / `restart`), attach-if-exists
  reuse, idle-timeout cleanup, and an in-VM-state-persistence policy call.
  *(was the deferred old Phase 8.)* Effort: L.

## D. Original-only features — decisions made

Decided 2026-05-30 with the user, per-feature.

### Will port back (now roadmap items)

- **D1 — `copilot` agent + Copilot token.** Add an `agent-vm copilot`
  subcommand, route Copilot token acquisition through the same host-rooted /
  proxy-substituted secret flow as the other agents, and install the Copilot
  CLI in the image. Original: `copilot_token.py`, `_copilot_vm_write_token`
  (claude-vm.sh:1269), `_copilot_vm_setup_home` (:1345),
  `_claude_vm_get_copilot_token` (:1537). Effort: M.
- **D2 — LSP plugins in the image.** Install the four language servers the
  original's `setup` adds — `clangd-lsp`, `pyright-lsp`, `typescript-lsp`,
  `gopls-lsp@claude-plugins-official` (claude-vm.sh:353-361) — at image-build
  time so in-VM Claude has code intelligence for C/C++, Python, TS, Go. Just
  Dockerfile + pre-warm. Effort: S.

### Won't do (confirmed non-goals)

- **GitHub App per-repo token minting** (`github_app_token_demo.py`,
  `_claude_vm_get_github_token`). The proxy allow-list already constrains
  pushes to cwd-derived repos; per-repo minting would add a GitHub App + device
  flow for marginal extra scoping. Keep `gh auth token` + allow-list.
- **USB passthrough** (`--usb`, `_agent_vm_usb_*` + qemu wrapper). libkrun is a
  minimal VMM with no qemu-style device-passthrough path. Hard architectural
  non-goal.
- **Dynamic memory / balloon** (`balloon-daemon.py`, `memory` subcommand,
  `--max-memory`). Short-lived 2 GB microVMs torn down per launch don't squat
  host RAM the way persistent 16 GB Lima VMs did, so ballooning is moot.

Obsolete-by-architecture (no decision needed): setup `--minimal` / `--disk`
(image is Docker-built once, not provisioned per-setup), `--max-memory` (tied
to the balloon).

## Discovered upstream issues (still open)

Carried over from the old plan; the IRQ/split-irqchip one (#3) is resolved.

1. `PullPolicy::Always` doesn't refresh the cached manifest digest — worked
   around with our own marker file.
2. `LayerDownloadProgress` events elided for fast registries.
4. No `Image::resolve(reference) -> RemoteRef` helper; we do raw-HTTP HEAD to
   ask the registry what's current.
5. IPv6 gateway DNS unresponsive in at least one libkrun config (see B3).
6. `exec_with`'s `StdinMode::Null` doesn't read as `/dev/null` to every client
   (codex blocks); worked around in the bash prelude.
7. High-level `exec_with` is buffer-until-exit only; switched to
   `exec_stream_with`.
8. Long secret placeholders (>~few hundred bytes) break sandbox boot at the
   runtime handshake; keep synthetic JWTs minimal.

## Working agreements

1. **One feature = one PR.** Stop after each; the user signs off.
2. **ARCHITECTURE.md is the source of truth for the *why*.** Every nontrivial
   design choice gets a short subsection: chosen / rejected / why.
3. **microsandbox changes go into the submodule**, on a branch of
   `wirenboard/microsandbox`, never vendored copies. Merge the submodule
   branch before the superproject (see AGENTS.md).
4. **Bump the workspace version on every merge** into `rewrite-microsandbox`
   (see AGENTS.md).
5. **Don't relocate build output to tmpfs.** Fix the root cause.
6. **Don't touch the old Bash `agent-vm`** on `main` until v1 ships from this
   branch.
