#!/bin/sh
# claude-wrapper.sh — sets LD_PRELOAD + dispatcher path, then exec's the real claude.
#
# Both env vars are inherited by every child process the daemon spawns,
# including the per-session `--bg-spare` workers. The shim intercepts those.
#
# CLAUDE_VM_SHIM_REAL_CLAUDE  : path to the real claude binary to exec
# CLAUDE_VM_SHIM_SO           : path to libclaude_vm_shim.so
# CLAUDE_VM_SHIM_DISPATCHER   : path to claude-vm-dispatcher
# CLAUDE_VM_SHIM_LOG          : if set, debug log path (shim + dispatcher)

set -e

: "${CLAUDE_VM_SHIM_REAL_CLAUDE:?CLAUDE_VM_SHIM_REAL_CLAUDE not set}"
: "${CLAUDE_VM_SHIM_SO:?CLAUDE_VM_SHIM_SO not set}"
: "${CLAUDE_VM_SHIM_DISPATCHER:?CLAUDE_VM_SHIM_DISPATCHER not set}"

# Append to any existing LD_PRELOAD (space-separated per ld.so).
if [ -n "$LD_PRELOAD" ]; then
    export LD_PRELOAD="$LD_PRELOAD $CLAUDE_VM_SHIM_SO"
else
    export LD_PRELOAD="$CLAUDE_VM_SHIM_SO"
fi

export CLAUDE_VM_SHIM_DISPATCHER
[ -n "$CLAUDE_VM_SHIM_LOG" ] && export CLAUDE_VM_SHIM_LOG

exec "$CLAUDE_VM_SHIM_REAL_CLAUDE" "$@"
