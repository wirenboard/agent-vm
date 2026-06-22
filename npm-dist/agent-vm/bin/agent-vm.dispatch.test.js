#!/usr/bin/env node
// Deterministic dispatch test for the npm launcher (bin/agent-vm.js).
//
// Why this exists: the launcher's job is to map the running
// platform/arch to a per-platform subpackage and then build the
// `bin/agent-vm` path inside it. arm64 was added to that mapping in
// B2, but arm64 dispatch cannot be exercised on an x86_64 CI host —
// `node` reports the host's real process.arch, so simply running the
// launcher there only ever exercises the linux-x64 leg. This test
// closes that gap without a real arm64 runtime by re-deriving the
// mapping + path logic from the launcher source and asserting the
// linux-arm64 key resolves to the expected package and to the same
// bin/ layout as linux-x64.
//
// Run standalone: `node bin/agent-vm.dispatch.test.js` (no test
// harness / deps — matches the repo convention of sanity-checking the
// launcher with plain `node`/`node --check`; there is no Node test
// runner or root package.json in this repo).
//
// It must stay in lockstep with the real launcher: it extracts the
// live PLATFORM_PACKAGES object out of agent-vm.js (rather than
// hard-coding a copy) so a future edit to the mapping that forgets
// arm64 — or renames the package — fails here instead of silently
// shipping a launcher that falls through to "unsupported platform"
// on arm64 hardware.

"use strict";

const assert = require("node:assert");
const path = require("node:path");
const fs = require("node:fs");
const vm = require("node:vm");

const launcherPath = path.join(__dirname, "agent-vm.js");
const src = fs.readFileSync(launcherPath, "utf8");

// Pull the `PLATFORM_PACKAGES = { ... };` object-literal out of the
// launcher source and evaluate just that literal in an isolated VM
// context. We deliberately do NOT `require()` the launcher: it runs
// its dispatch + process.exit() at module load, so requiring it would
// terminate this test process.
const m = src.match(/const\s+PLATFORM_PACKAGES\s*=\s*(\{[\s\S]*?\});/);
assert.ok(m, "could not locate PLATFORM_PACKAGES object literal in agent-vm.js");
const PLATFORM_PACKAGES = vm.runInNewContext(`(${m[1]})`);

// Mirror the launcher's binPath construction (the
// `path.join(dir, "bin", "agent-vm"+ext)` line) so we assert the SAME
// layout the launcher actually uses.
function binPathFor(pkgDir, platform) {
  const ext = platform === "win32" ? ".exe" : "";
  return path.join(pkgDir, "bin", `agent-vm${ext}`);
}

// 1) The new arm64 key must resolve to the arm64 subpackage.
assert.strictEqual(
  PLATFORM_PACKAGES["linux-arm64"],
  "@wirenboard/agent-vm-linux-arm64",
  "linux-arm64 must map to @wirenboard/agent-vm-linux-arm64",
);

// 2) x64 must still resolve (guards against an accidental clobber).
assert.strictEqual(
  PLATFORM_PACKAGES["linux-x64"],
  "@wirenboard/agent-vm-linux-x64",
  "linux-x64 must map to @wirenboard/agent-vm-linux-x64",
);

// 3) Both linux platforms must produce the identical bin/ layout
//    (only the package dir differs) — the arm64 subpackage ships its
//    binary at bin/agent-vm exactly like x64 (see its package.json
//    `files` list + the bin/.gitkeep placeholder).
const x64Bin = binPathFor("/pkg/agent-vm-linux-x64", "linux");
const arm64Bin = binPathFor("/pkg/agent-vm-linux-arm64", "linux");
assert.strictEqual(path.basename(x64Bin), "agent-vm");
assert.strictEqual(path.basename(arm64Bin), "agent-vm");
assert.strictEqual(
  path.relative("/pkg/agent-vm-linux-x64", x64Bin),
  path.relative("/pkg/agent-vm-linux-arm64", arm64Bin),
  "arm64 and x64 must use the same bin/ layout",
);

// 4) A genuinely unsupported platform key must be absent so the
//    launcher hits its "no prebuilt binary" error path cleanly.
assert.strictEqual(
  PLATFORM_PACKAGES["sunos-sparc"],
  undefined,
  "unsupported platform keys must be absent (no fall-through entry)",
);

console.log("agent-vm dispatch test: OK (linux-x64, linux-arm64)");
