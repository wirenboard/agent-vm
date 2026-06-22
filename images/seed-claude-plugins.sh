#!/bin/sh
# Seed the image's baked Claude LSP plugins into the persistent state dir on
# first boot.
#
# The image installs the plugins into /root/.claude/plugins at build time, but
# the launcher symlinks /root/.claude -> /agent-vm-state/claude for persistence
# (run.rs), which shadows the baked tree so `claude plugin list` sees nothing.
# This script (called from the launcher prelude before the agent execs) copies
# the stash into the state dir once and merges the enabledPlugins +
# extraKnownMarketplaces keys into the state settings.json, preserving anything
# the user already set. Best-effort: never fails the launch.
#
# Idempotent: a present /agent-vm-state/claude/plugins means we already seeded
# (or the user manages plugins themselves) — do nothing.
SEED=/opt/agent-vm/claude-seed
STATE=/agent-vm-state/claude

[ -d "$SEED/plugins" ] || exit 0
[ -e "$STATE/plugins" ] && exit 0

mkdir -p "$STATE" 2>/dev/null
cp -a "$SEED/plugins" "$STATE/plugins" 2>/dev/null || exit 0

if [ -f "$SEED/settings.json" ] && command -v node >/dev/null 2>&1; then
  node -e '
const fs = require("fs");
// With `node -e CODE a b`, user args start at argv[1] (no script path slot).
const [, statePath, seedPath] = process.argv;
const seed = JSON.parse(fs.readFileSync(seedPath, "utf8"));
let st = {};
try { st = JSON.parse(fs.readFileSync(statePath, "utf8")); } catch (e) {}
st.enabledPlugins = Object.assign({}, seed.enabledPlugins || {}, st.enabledPlugins || {});
st.extraKnownMarketplaces = Object.assign({}, seed.extraKnownMarketplaces || {}, st.extraKnownMarketplaces || {});
fs.writeFileSync(statePath, JSON.stringify(st, null, 2));
' "$STATE/settings.json" "$SEED/settings.json" 2>/dev/null || true
fi

exit 0
