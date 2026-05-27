#!/bin/sh
# Install the shim+dispatcher to /opt/claude-vm-shim and a wrapper at
# ~/.local/bin/claude-shimmed for safe testing alongside the real claude.
#
# Usage:
#   ./install.sh            # installs as `claude-shimmed`
#   ./install.sh --replace  # ALSO repoints ~/.local/bin/claude to the wrapper
#                           # (back up the original symlink first)
#
# Run from this script's directory.

set -eu

here="$(cd "$(dirname "$0")" && pwd)"
target_root="/opt/claude-vm-shim"
bindir="${HOME}/.local/bin"

# Make sure we've built first.
shim="${here}/target/release/libclaude_vm_shim.so"
dispatcher="${here}/target/release/claude-vm-dispatcher"
bridge="${here}/target/release/claude-vm-bridge"
if [ ! -x "$shim" ] || [ ! -x "$dispatcher" ] || [ ! -x "$bridge" ]; then
    echo "build artifacts missing — run \`cargo build --release\` from ${here} first" >&2
    exit 1
fi

# Resolve the real claude — wrapper will exec this.
real_claude="$(readlink -f "${bindir}/claude" 2>/dev/null || true)"
case "$real_claude" in
    *claude-shimmed*|"")
        echo "ERR: ${bindir}/claude is missing or already points at our wrapper." >&2
        echo "Set CLAUDE_VM_SHIM_REAL_CLAUDE manually if testing." >&2
        exit 1
        ;;
esac

echo "real claude:      $real_claude"
echo "target root:      $target_root"

mkdir -p "$target_root/bin" "$target_root/lib" "$bindir"
install -m 0755 "$shim" "$target_root/lib/libclaude_vm_shim.so"
install -m 0755 "$dispatcher" "$target_root/bin/claude-vm-dispatcher"
install -m 0755 "$bridge" "$target_root/bin/claude-vm-bridge"

# Render the wrapper with bound paths so users can run it without setting envs.
cat > "$target_root/bin/claude-shimmed" <<EOF
#!/bin/sh
export CLAUDE_VM_SHIM_REAL_CLAUDE="${real_claude}"
export CLAUDE_VM_SHIM_SO="${target_root}/lib/libclaude_vm_shim.so"
export CLAUDE_VM_SHIM_DISPATCHER="${target_root}/bin/claude-vm-dispatcher"
exec "${here}/wrapper/claude-wrapper.sh" "\$@"
EOF
chmod 0755 "$target_root/bin/claude-shimmed"

ln -sf "$target_root/bin/claude-shimmed" "$bindir/claude-shimmed"
echo "installed:        $bindir/claude-shimmed"

if [ "${1:-}" = "--replace" ]; then
    backup="$bindir/claude.orig-$(date +%s)"
    if [ -L "$bindir/claude" ]; then
        mv "$bindir/claude" "$backup"
        echo "backed up:        $backup"
    fi
    ln -sf "$target_root/bin/claude-shimmed" "$bindir/claude"
    echo "replaced:         $bindir/claude (was → $real_claude)"
fi

echo
echo "Run:"
echo "  CLAUDE_VM_SHIM_LOG=/tmp/cvs.log claude-shimmed --version"
echo "to verify the wrapper works, then check /tmp/cvs.log for any dispatcher hits."
