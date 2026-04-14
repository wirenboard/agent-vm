#!/usr/bin/env bash
#
# agent-vm-wsl.sh: Backwards-compatibility shim for WSL2 users.
#
# WSL2 support is now built directly into claude-vm.sh, which auto-detects
# WSL2 via /proc/version. This file exists only so that existing shell configs
# that source agent-vm-wsl.sh continue to work without modification.
#
# Source this file in your WSL2 shell config (~/.bashrc) — or switch to:
#   source /path/to/agent-vm/claude-vm.sh
#
source "$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)/claude-vm.sh"
