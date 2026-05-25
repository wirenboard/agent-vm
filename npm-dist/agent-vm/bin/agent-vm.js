#!/usr/bin/env node
// Tiny launcher: resolve the platform-specific subpackage, find its
// `agent-vm` binary, and exec it with our argv. The platform
// subpackages also bundle the patched `msb` binary and libkrunfw
// alongside `agent-vm` — agent-vm itself discovers them via
// `current_exe()`-relative paths, so the launcher only needs to
// find the main binary.
//
// Why a JS launcher at all? npm's `bin` field expects a JS or
// shell entrypoint. We can't put the native binary directly here
// because npm doesn't know how to install per-platform native
// binaries from a single package; the optionalDependencies +
// per-platform subpackages pattern (esbuild / ruff / biome) is
// the supported way, and that requires a launcher that picks the
// right subpackage at runtime.

"use strict";

const { spawnSync } = require("node:child_process");
const path = require("node:path");
const fs = require("node:fs");

const PLATFORM_PACKAGES = {
  "linux-x64": "@wirenboard/agent-vm-linux-x64",
  // "linux-arm64": "@wirenboard/agent-vm-linux-arm64",
  // "darwin-arm64": "@wirenboard/agent-vm-darwin-arm64",
  // "darwin-x64": "@wirenboard/agent-vm-darwin-x64",
  // "win32-x64": "@wirenboard/agent-vm-win32-x64",
};

const platformKey = `${process.platform}-${process.arch}`;
const pkg = PLATFORM_PACKAGES[platformKey];
if (!pkg) {
  console.error(
    `agent-vm: no prebuilt binary for ${platformKey}. ` +
      `Supported: ${Object.keys(PLATFORM_PACKAGES).join(", ")}. ` +
      `Build from source: https://github.com/wirenboard/agent-vm`
  );
  process.exit(1);
}

let binPath;
try {
  // require.resolve finds the subpackage's package.json; the
  // binary lives at `bin/agent-vm` next to it.
  const pkgJson = require.resolve(`${pkg}/package.json`);
  const ext = process.platform === "win32" ? ".exe" : "";
  binPath = path.join(path.dirname(pkgJson), "bin", `agent-vm${ext}`);
} catch {
  // Most common causes, ordered by likelihood:
  //  1. Transient network failure during `npm install`: the main
  //     package installs, the optional dep silently fails. npm logs
  //     a warning but it's easy to miss.
  //  2. `npm install --no-optional` (or yarn `--ignore-optional`).
  //  3. Lock-file pins to a version of the main package without a
  //     matching subpackage version on npm (mid-publish).
  console.error(
    `agent-vm: the prebuilt binary subpackage ${pkg} is not installed.\n` +
      `  - If your last \`npm install\` showed a warning about ${pkg} failing, that's the cause —\n` +
      `    retry with \`npm install -g @wirenboard/agent-vm --force\` (network was likely flaky).\n` +
      `  - If you passed \`--no-optional\` / \`--ignore-optional\`, re-install without it.\n` +
      `  - If you locked an inconsistent set of versions, delete your lockfile entry and re-resolve.`
  );
  process.exit(1);
}

if (!fs.existsSync(binPath)) {
  console.error(`agent-vm: expected binary at ${binPath} but it is missing.`);
  process.exit(1);
}

// Inherit stdio so the user sees the binary's output directly.
// argv[0]=node, argv[1]=this script — forward only argv[2..].
const result = spawnSync(binPath, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`agent-vm: failed to exec ${binPath}: ${result.error.message}`);
  process.exit(1);
}
// Mirror the child's exit status / signal back to our parent.
if (result.signal) {
  // Re-raise the same signal so callers see e.g. SIGINT, not exit 0.
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 1);
