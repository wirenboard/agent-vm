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
  console.error(
    `agent-vm: optionalDependency ${pkg} did not install. ` +
      `Re-install with \`npm install -g @wirenboard/agent-vm --force\` ` +
      `or check that --no-optional was not passed.`
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
