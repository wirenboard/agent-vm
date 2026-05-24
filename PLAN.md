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
- Project working directory mounted into the sandbox at the project's host
  path (with `/workspace` fallback for tmpfs-rooted paths).
- Per-project session persistence for `~/.claude/`, `~/.codex/`,
  `~/.local/share/opencode/` under `${XDG_STATE_HOME}/agent-vm/<project-hash>/`.
- Host-rooted credentials with refresh: real tokens never enter the VM; host
  `claude -p` / `codex exec` are used to rotate; the VM picks up the new token
  on the next request without restarting the sandbox. Covers Claude, Codex,
  and OpenCode (which auths against OpenAI like Codex does).
- Pre-baked Debian-based OCI image with the three agent CLIs and dev tools.
- Interactive attach for the agent TUIs.
- Host `gh` / `git` auth reused inside the guest with **per-launch repo
  allow-list**: agents can `git push` and `gh pr create` only to repos
  derived from the cwd's remote(s) plus `--repo owner/name` overrides; the
  request-interceptor hook returns synthesized 403s for any other repo.
- Security snapshot of host credential files: detect unexpected mutations
  that aren't from the Phase 4 refresh hook.
- `--mount HOST:GUEST` for additional host directories.
- Clipboard bridge between host and guest.
- `agent-vm-ccusage` wrapper that unions per-project Claude session dirs.
- Chrome DevTools MCP — Chromium in the image, MCP config injected into the
  agents' settings (so the in-VM agent can drive a real headless browser).

## v1 scope (out, may revisit)

- GitHub App device flow + per-repo scoped tokens (we reuse the user's
  existing `gh` auth instead — see "v1 scope (in)" above).
- GitHub Copilot CLI subcommand and Copilot token acquisition.
- USB passthrough.
- Dynamic memory / virtio-balloon daemon.
- `AI_HTTPS_PROXY` upstream proxy chaining.
- Apple Silicon / macOS-VZ specifics.
- WSL2-on-Windows specifics.
- Setup-time `--minimal` / `--disk` flags (image is built once, fixed shape).

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

### Phase 3 — Static host-rooted secrets [done — submodule branch `agent-vm-secret-file`, committed `8cc036b`]

The big architectural payoff of moving to microsandbox: real tokens
never enter the VM.

What shipped:

- **Upstream extension** on a vendor/microsandbox branch:
  `SecretValue { Static(String), File(PathBuf) }` with a bare-string
  wire format for Static (backward-compatible with prebuilt `msb`) and a
  NUL-prefixed sentinel for File (deferred Phase 4 plumbing). 250
  microsandbox-network tests green; new tests cover both wire formats.
- **agent-vm secrets module**: per-launch snapshot of
  `~/.claude/.credentials.json` and `~/.codex/auth.json`, atomic write
  of placeholder credentials files into the per-project state dir.
- **Launcher wiring**: TLS interception enabled, file-backed allow-host
  configured for both providers (Anthropic + OpenAI), real access
  tokens passed via `SecretValue::Static(...)` (Phase 3) — File
  variant is forward-looking infra used only by Phase 4 once a patched
  msb ships.
- `IS_SANDBOX=1` set so Claude Code's "don't run as root" guard yields
  to the fact that the microVM *is* the security boundary.

What we deliberately punted:

- **Refresh**. Long sessions will eventually 401. Phase 4 handles it.
- **`~/.microsandbox/bin/msb` rebuild**. Required for `SecretValue::File`
  to behave (without it, an old msb substitutes the literal sentinel
  string). Phase 4 ships the replacement and switches to File-backed
  secrets.
- **OAuth refresh endpoint MITM** (forging
  `platform.claude.com/v1/oauth/token` responses from the host file).
  Original Bash agent-vm does this; Phase 4 here.

**Done when:** inside the guest, `cat /proc/1/environ | tr '\0' '\n' |
grep -i token` shows only placeholders, while a Claude API request
through `api.anthropic.com` goes through the microsandbox CA-signed
intercept proxy with the placeholder substituted in the Authorization
header. **Actual:** verified at the network layer (TLS cert chain shows
`CN=microsandbox CA`, debug config dump shows the substituted real
token); the final "Anthropic returns a real response" leg can't be
verified on the nested test host because of an outer credential bridge
that itself substitutes placeholders. Structurally equivalent to the
original Bash agent-vm's credential-proxy flow.

### Phase 4 — Refresh semantics [done — committed `8e262c0`..`85ffd34`; Codex+Claude verified end-to-end against real credentials, leaks fixed]

Tokens rotate; long-running sandbox sessions must survive that without
re-attaching. Phase 3 makes the access token swappable in principle;
this phase teaches agent-vm to actually do the swap, with the simplest
moving parts that work.

Design:

- **Rebuild `~/.microsandbox/bin/msb` from our fork** so
  `SecretValue::File` actually re-reads the host token file on every
  connection-setup. With this in place, the proxy always picks up the
  current host file content without any host-side daemon.
- **MITM the OAuth refresh endpoint** (`platform.claude.com/v1/oauth/
  token`, `auth.openai.com/oauth/token`). When the in-VM agent tries
  to refresh:
  1. agent-vm spawns a host-side `claude -p "ping" --model sonnet` /
     `codex exec --skip-git-repo-check "Reply OK"` to trigger the
     host CLI to rotate its credential file (this is what the
     original Bash agent-vm does).
  2. Re-reads the host file.
  3. Synthesizes the refresh-endpoint response from the host's new
     `accessToken` / `expiresAt`, but with placeholder strings for
     the body's `access_token` field — so the in-VM agent's local
     credentials file is updated to a placeholder, not the real
     token. Same shape as the rest of the substitution flow.
  4. The next API request from the in-VM agent uses the new
     placeholder, which `SecretValue::File` swaps for the real
     newly-rotated token. No restart, no manual intervention.
- **Single-flight** for the host-CLI invocation so two concurrent
  in-VM refresh attempts don't fire two host-side `claude -p`
  processes at once.

What we **don't** need (per discussion): a proactive "token nearing
expiry" timer. The guest's own refresh attempt at 401-time is the
trigger, and the MITM handles it. If the user already ran `claude` on
the host between refreshes and the host file is fresh, the in-VM
substitution picks it up on the next request without any of this
machinery firing — `SecretValue::File` is the whole story for the
externally-rotated case.

**Done when:** a multi-hour session crosses a token rotation
end-to-end without the agent seeing an auth error and without manual
intervention.

**Verification session (2026-05-21):** stood the runtime back up on a
fresh host (installed `libkrunfw` bundle + the patched
`0.4.6+agent-vm.phase4` msb), rebuilt the image, and ran the launcher
against a real host Claude credential. Findings:

- **`images/build.sh` registry bug fixed.** Docker 29.x emits a stray
  blank line to *stdout* on `docker inspect` of a missing container, so
  `state=$(... || echo missing)` became `"\nmissing"` and never matched
  the `missing` case — the script tried to `docker start` a nonexistent
  container. Now whitespace-stripped with an empty→missing fallback.
- **Real-token leak into the guest found + fixed.** The token files
  were written under `state_dir`, which is bind-mounted into the guest
  at `/agent-vm-state`, so `cat /agent-vm-state/tokens/anthropic`
  returned the host bearer. Moved to a host-only sibling
  `<hash>.secrets/` (0700), never mounted; added a guard test. See
  ARCHITECTURE.md "Token files live outside the guest bind mount".
- **Network layer verified.** Inside the guest, `credentials.json` and
  PID1 environ show only placeholders; `api.anthropic.com`'s server
  cert is issued by `CN=microsandbox CA` (traffic goes through the
  intercept proxy). The final real-response leg still can't be checked
  here — this is a doubly-nested host whose *outer* agent-vm bridge
  already replaced the host token with its own placeholder
  (`sk-ant-oat01-placeholder-proxy-managed`) and doesn't re-intercept
  the nested VM's egress, so Anthropic returns 401. Same documented
  limitation as Phase 3.
- **Codex path not exercisable here:** no `~/.codex/auth.json` on this
  host, so the OpenAI/Codex websocket flow (the original stop point)
  can't be authenticated. The `chatgpt.com` WebSocket support
  (`inject_basic_auth(false)` + zero-copy fast path) is in place and
  the codex CLI (now 0.133.0) is present in the verified image, but a
  host with real Codex credentials is needed to confirm it end-to-end.

**Verification session (2026-05-24):** the user ran `codex login` to
populate `~/.codex/auth.json`, and we drove the full Codex flow until
it actually returned a real gpt-5.5 response. Three additional fixes
landed:

- **`id_token` JWT leak in `<state>/codex/auth.json`.** `secrets.rs`
  was substituting `access_token` and `refresh_token` but leaving the
  OpenAI `id_token` JWT verbatim — that JWT decodes to user email,
  chatgpt account id, plan type, org list, user_id. Replaced the
  static `OPENAI_ID_PLACEHOLDER` string with a structurally valid
  alg-none JWT carrying clearly-fake fields, so codex 0.133's
  client-side JWT parse succeeds and no PII enters the guest. Leak
  grep for the host email, real access/refresh/id token prefixes
  inside the guest mount: all absent.
- **IPv6 nameserver in `/etc/resolv.conf` hung codex's resolver.**
  microsandbox's agentd writes both v4 and v6 gateway DNS at boot. In
  this nested-libkrun config the v6 gateway times out on UDP/53
  queries; glibc's `getaddrinfo` silently skips it and uses v4, but
  codex's Rust async resolver returns `EAI_AGAIN` and fails. `getent
  hosts chatgpt.com` returned immediately while codex hung at "failed
  to lookup address information". `run.rs` now wraps the agent
  command in a tiny bash prelude that `sed`s the colon-bearing
  nameserver line out of `/etc/resolv.conf` before exec.
- **codex 0.133 `exec` blocks on stdin unless it's `/dev/null`.**
  `exec_with`'s `StdinMode::Null` was not enough; codex waited
  indefinitely for what it thought was unbounded interactive input.
  Backgrounding (`&` in bash) worked because that auto-redirects
  stdin. The prelude now does `[ -t 0 ] || exec < /dev/null` so
  interactive TTY launches are unaffected.
- **Streaming output** in non-TTY mode (switched the launcher from
  `exec_with` to `exec_stream_with`). Long-running agent commands
  used to look completely silent until exit; now stdout/stderr stream
  live and partial output survives Ctrl-C / timeout. Independent of
  the codex fix but uncovered by the same debugging.

End-to-end on a real `gpt-5.5` host credential: `agent-vm codex exec
--skip-git-repo-check "Reply with: CODEX_DIRECT_OK"` returns
`CODEX_DIRECT_OK` in ~7 s (boot 2.4 s + run 4.3 s), with no host
token, refresh token, id_token JWT or user PII anywhere under
`/agent-vm-state`. Claude path was re-tested and still hits the
documented nested-host 401; that's not a regression — same outer-
bridge limit as Phase 3.

**Still untested before Phase 4 can claim its "Done when":**

- **Actual mid-session rotation.** The substitution + refresh-hook
  *infrastructure* is verified, but no run has yet crossed a real
  token-expiry boundary and survived. The OpenAI ChatGPT access token
  lives ~24 h; we need a long-running session that goes through at
  least one rotation event without re-attaching. The hook code path
  (host CLI rotates → re-read file → synthesize placeholder response
  → guest writes placeholder → next request uses fresh real token)
  has not actually fired in anger.
- **Single-flight on the host-CLI invocation.** Listed under
  "Design" but not yet implemented in `intercept_hook.rs` — two
  concurrent in-guest refresh attempts could each spawn `claude -p`
  or `codex exec` on the host. Host CLI's own file lock prevents
  corruption, so the worst case is one extra rotation invocation,
  but it's worth a `<state>.secrets/.refresh.lock` flock once we see
  it bite.

### Phase 5 — OpenCode auth + security snapshot [pending]

Two small completions of the auth/secret story:

**OpenCode auth.** Phase 4 covered Claude and Codex; OpenCode is the
third in-scope agent and was deferred. OpenCode authenticates against
OpenAI but uses its own file shape — original is at
`claude-vm.sh:996` (`_opencode_vm_build_oauth_auth_json`):
`{type:"oauth", refresh, access, expires, accountId}` where `access`
is a JWT with `iss/aud/exp/scp/chatgpt_account_id/chatgpt_plan_type`.

Extend `secrets.rs`:

- Read `~/.local/share/opencode/auth.json` if it exists; otherwise derive
  the account/email/plan fields from `~/.codex/auth.json` (same OpenAI
  account, different on-disk format).
- Write a placeholder `<state>/opencode/auth.json` whose `access` is a
  synthetic alg-none JWT carrying placeholder-only payload fields (same
  pattern as Phase 4's `OPENAI_ID_PLACEHOLDER`). `refresh` is a static
  placeholder string.
- Register the real OpenAI access token as a second `SecretValue::File`
  entry keyed off a distinct placeholder so api.openai.com /
  chatgpt.com requests from OpenCode get substituted (same allow-host
  set as Codex).

**Security snapshot.** Cheap safety net for "did agent-vm itself, or a
bug in the refresh hook, mutate my host tokens in some way I didn't
expect?" At launcher start, take SHA-256 of
`~/.claude/.credentials.json`, `~/.codex/auth.json`,
`~/.local/share/opencode/auth.json`; on sandbox exit, re-hash and warn
if any of them changed outside the Phase 4 refresh-hook path (which we
*do* expect to mutate them). Original is `claude-vm.sh:1560`
(`_claude_vm_security_snapshot/check`).

**Done when:** OpenCode authenticates to OpenAI through the proxy on a
real host (analogous to the Codex e2e in Phase 4 verification 2026-05-
24); the security snapshot fires on a synthetic mid-run mutation.

### Phase 6 — gh / git credential injection + per-launch repo allow-list [pending]

Without this the in-VM agent can read the project but can't `git push`,
can't `gh pr create`, can't fetch a private dependency from GitHub.
With it, agents become useful for actual development work.

**Design:**

1. **Reuse host gh auth — don't mint new tokens.** Read `gh auth token`
   (or parse `~/.config/gh/hosts.yml`) on the host at launch. Register
   it as a `SecretValue::File` (same primitive Phase 4 uses for
   Claude/Codex) so a host-side `gh auth refresh` propagates to the
   guest without a relaunch. The in-VM `gh` / `git` see a placeholder
   token; microsandbox's TLS-intercept proxy substitutes on the way
   out. Allow-host set: `api.github.com`, `github.com`, `codeload.
   github.com`, `raw.githubusercontent.com`, `objects.githubusercontent.
   com`, plus the SSH endpoint for HTTPS pushes that the gh credential
   helper handles.

2. **Per-launch repo allow-list — enforced at the proxy.** A real gh
   OAuth token typically has `repo` scope (read+write to every repo
   the user can see). We don't want an off-rails agent pushing to all
   of them. So:

   - Build the allow-list at launch:
     - Parse `git remote -v` in the cwd (logic ports from
       `_claude_vm_parse_github_remote` at `claude-vm.sh:1432`).
     - Append any `--repo owner/name` overrides from the CLI
       (repeatable).
     - `--no-git` skips the whole gh path (no token, no hosts.yml,
       no allow-list).
   - Extend the request-interceptor hook (`crates/agent-vm/src/
     intercept_hook.rs`) with a third route family: `api.github.com`
     and friends. Phase 4's hook fires on `(host, method, path
     prefix)`; for GitHub we match on host=`api.github.com`,
     any-method, all paths, and inside the hook reject anything
     whose path doesn't start with `/repos/<allowed-owner>/<allowed-
     repo>/`, `/user`, `/user/repos`, `/orgs/<allowed-org>/`,
     `/notifications` (read-only), etc. Denied requests return a
     synthesized 403 with a clear body so the in-VM `gh`/`git`
     surfaces a comprehensible error instead of a hang or 5xx.
   - `codeload.github.com` / `raw.githubusercontent.com` filter on
     `/<allowed-owner>/<allowed-repo>/...` similarly.

3. **In-guest config injection.** Write `~/.gitconfig` (uses the gh
   credential helper that forwards to placeholder token) and
   `~/.config/gh/hosts.yml` (placeholder token, real user). Same shape
   as `_claude_vm_inject_git_credentials` + `_inject_gh_credentials`
   at `claude-vm.sh:682,706`.

4. **CLI flags.** `--no-git` (skip everything), `--repo OWNER/NAME`
   (repeatable; allow-list addition).

**Open question:** the OAuth proxy hook in Phase 4 sees buffered
plaintext HTTP request bytes via stdin — that's what we need for
path-based filtering too. Confirm `intercept/handler.rs` rule
matching supports "any method, any path" wildcards or extend it.

**What we explicitly are NOT doing** (per user direction):
- No GitHub App device flow.
- No per-repo scoped token minting.
- No Copilot CLI / Copilot API plumbing.

**Done when:** `agent-vm claude -p "...do work then commit and push..."`
in a real GitHub project lands a commit on the remote; an agent attempt
to push to a *different* repo gets a clean 403 from the proxy hook
rather than reaching GitHub.

### Phase 7 — DX additions: `--mount`, clipboard, ccusage, Chrome DevTools MCP [pending]

A grab-bag of original-agent-vm capabilities the user wants in v1.
Each is independent and lands as its own PR per the working agreement.

**`--mount HOST:GUEST`** (repeatable). Pass extra host directories
through to the guest as bind mounts. The launcher already mounts the
project at its host path + the per-project state dir at
`/agent-vm-state`; add user-supplied extras. **Mind the libkrun
virtio-IRQ cap** (~6 devices; see Discovered Upstream Issue #3) — print
a clear error if the user crosses it, *don't* silently fail at boot.

**Clipboard bridge.** Original `clipboard-pty.py` does a live PTY
bridge; that's more than we need. v1 design: a per-project
`<state>/clipboard.{txt,png}` bind-mounted into the guest at a known
path (e.g. `/agent-vm-state/clipboard.*`), plus:
- In-guest helper `/usr/local/bin/agent-vm-clip` (baked into the image)
  that reads/writes those files.
- Host-side subcommand: `agent-vm clipboard get|put` which exchanges
  with the host clipboard via `xclip` / `wl-copy` / `pbpaste`,
  resolving the active sandbox's state dir from cwd. Defer live two-way
  sync — the file-based pull/push covers "agent emits a code block,
  I copy it into another app" and vice versa.

**`agent-vm-ccusage` wrapper.** Port verbatim from `bin/ccusage` in
the original (4 lines): set `CLAUDE_CONFIG_DIR` to the comma-joined
union of `~/.claude` + every per-project session dir under
`${XDG_STATE_HOME}/agent-vm`, then `exec npx ccusage@latest`. Ship as
a separate shell script in `bin/` and reference from README.

**Chrome DevTools MCP.** Original at `claude-vm.sh:385` installs
Chromium into the image with `google-chrome` symlinks, then writes the
MCP server entry into `~/.claude.json` / Codex settings:

```json
"chrome-devtools": {
  "command": "npx",
  "args": ["-y", "chrome-devtools-mcp@latest", "--headless=true", "--isolated=true"]
}
```

In the rewrite:
- Add `chromium` + the `google-chrome` symlink dance to `images/
  Dockerfile`.
- Extend `secrets::write_default_claude_settings` (and equivalents for
  Codex `config.toml` / OpenCode `config.json`) to inject the
  MCP server entry into the merged settings. Merge semantics already in
  place: user-set MCP servers survive, the chrome-devtools entry is
  force-set.
- `AGENT_VM_NO_CHROME_MCP=1` opt-out for users who don't want
  Chromium running in the guest.

**Done when:** `--mount` round-trips a file into a guest path of the
user's choice; `agent-vm clipboard put` then in-guest `cat
/agent-vm-state/clipboard.txt` shows the same string; `agent-vm-
ccusage` reports usage across all project sessions; in-VM Claude
opens a real URL via the MCP and screenshots it.

### Phase 8 — Fast-launch (deferred — wrong instrument for the job)

Originally framed around `Sandbox::from_snapshot(...)` on the assumption
that microsandbox snapshots checkpoint VM memory (à la Firecracker's
snapshot/restore). Confirmed reading
`vendor/microsandbox/crates/microsandbox/lib/snapshot/mod.rs` that they
do **not**: a snapshot captures the stopped sandbox's writable upper
filesystem layer plus the metadata that pins the immutable lower
(image). Booting `from_snapshot` still goes through the full libkrun
kernel boot (~1.0 s) — the snapshot only saves the EROFS materialize
step on a re-pull, which we already pay only on explicit `agent-vm pull`
(rare). So filesystem snapshots are the wrong instrument for cutting
launch time.

The real lever for fast launches is **detached mode**: boot a sandbox
once per project, leave it running, attach for each subsequent
`agent-vm <agent>` invocation. Microsandbox exposes
`create_detached`, `start_detached`, and `Sandbox::get(name)` already.
Round-trip drops from ~1.5 s to ~10–50 ms but pulls in:

- New lifecycle subcommands (`agent-vm ps`, `agent-vm stop`,
  `agent-vm restart`).
- A reuse strategy for per-project sandbox names (we already have
  `agent-vm-<hash>`; just need `.replace()` to flip to "attach if
  exists, create-detached otherwise").
- Idle-timeout cleanup so abandoned sandboxes don't squat memory.
- A policy decision: does state inside the VM (`/tmp`, `/var/log`)
  persist between agent invocations? Today every launch is a fresh
  VM, so this is a behaviour change.

Deferred pending a clear product call. The architectural payoff of
microsandbox is keeping tokens out of the VM (Phase 3), and current
1.5 s launch is acceptable.

### Phase 9 — Distribution + polish + docs [pending]

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
- **Upstream-fix or formalize the IPv6 DNS workaround.** The launcher
  currently sed's the v6 nameserver out of `/etc/resolv.conf` every
  launch (see Phase 4 verification 2026-05-24, upstream issue #5).
  Either get the v6 gateway DNS path working in microsandbox or expose
  a config flag — landing one of those lets us drop the bash prelude
  back to just the stdin redirect.

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
5. **IPv6 gateway DNS is unresponsive in at least one libkrun config.**
   `agentd` writes both v4 and v6 gateway nameservers into
   `/etc/resolv.conf` (`crates/agentd/lib/network.rs:556`), but UDP/53
   queries to the v6 gateway time out while v4 works. glibc's
   `getaddrinfo` hides this by skipping the broken resolver; strict
   async resolvers (codex / hickory-style) hang with `EAI_AGAIN`. We
   work around it in agent-vm by sed'ing colon-bearing nameserver
   lines out of `/etc/resolv.conf` before exec'ing the agent. Right
   fix is either (a) make the v6 gateway DNS path actually work, or
   (b) expose a `network.dns(|d| d.disable_ipv6(true))` knob so the
   guest only sees v4 nameservers.
6. **`exec_with` default `StdinMode::Null` doesn't read as `/dev/null`
   to every client.** codex 0.133's `exec` subcommand blocks
   indefinitely on what it considers an open stdin pipe under
   `StdinMode::Null`, but reads EOF correctly when we explicitly
   redirect to `/dev/null` from inside the bash wrapper. Suggests the
   fd that gets handed to the in-guest process is something other than
   `/dev/null` (maybe a closed pipe, maybe a pipe that hasn't been
   closed on the sender side). Worth tracing what `StdinMode::Null`
   ends up as inside the guest.
7. **High-level `exec_with` is buffer-until-exit only.** It returns a
   completed `ExecOutput`, so a hung child plus an external timeout
   leaves the caller with zero observable output — making "is it
   stuck or just slow?" indistinguishable. We switched to
   `exec_stream_with` (which exists and works), but the wrapper API
   should probably stream by default and offer a `.collect()` adapter
   for the rare buffer-it-all case.
8. **Long secret placeholders break sandbox boot.** Registering a
   ~480-byte placeholder string (a JWT-shaped synthetic with full
   OpenAI auth claims) caused `runtime error: handshake read
   id_offset: timed out before relay sent bytes` at sandbox create
   time, before agentd ever runs in the guest. Same setup with the
   placeholder shrunk to ~150 bytes boots fine. Boot failure happens
   long before the substitution proxy is exercised, so the limit must
   sit in the config-delivery / runtime-handshake path rather than the
   secret scanner itself. Worth tracing — Phase 5 works around it by
   keeping OpenCode's synthetic JWT minimal (3-claim payload, short
   sig), but anything in agent-vm or downstream that wants placeholders
   above a few hundred bytes will silently fail.

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
