#!/bin/bash
# Wall-clock `create` per libkrunfw variant. Interleaved across variants to
# cancel host drift; drop_caches between rounds; mean/stdev. QUIET host only.
set -u
export PATH="/home/boger/work/board/tmp/agent-vm/.agent-vm-rust/rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:$PATH"
export AGENT_VM_NO_CHROME_MCP=1   # strip the run-phase certutil; we measure create only
export MSB_PATH=/home/boger/work/board/tmp/agent-vm/vendor/microsandbox/target/release/msb
AGENTVM=/home/boger/work/board/tmp/agent-vm/target/release/agent-vm
LIBSO=/home/boger/work/board/tmp/agent-vm/vendor/microsandbox/target/release/libkrunfw.so.5.2.1
VAR=/tmp/variants
PROJ=/root/profproj
VARIANTS=(stock stock_kvm heavy_nonf heavy_legacy heavy heavy_deferred)
ROUNDS=${1:-10}

cd "$PROJ" || exit 1
swap() { cp "$VAR/libkrunfw-$1.so.5.2.1" "$LIBSO" || exit 1; }
create() { local mem="$1"; AGENT_VM_PROFILE=1 "$AGENTVM" shell --memory "$mem" true 2>&1 \
  | sed 's/\x1b\[[0-9;]*m//g' | grep -oE 'create:[[:space:]]+[0-9.]+s' | grep -oE '[0-9.]+'; }
mstat() { tr ' ' '\n' <<<"$1" | awk 'NF{n++;v=$1;s+=v;ss+=v*v;if(mn==""||v<mn)mn=v;if(v>mx)mx=v} END{if(!n){print "no data";exit} m=s/n;d=ss/n-m*m;if(d<0)d=0;printf "mean=%.3f sd=%.3f min=%.3f max=%.3f n=%d",m,sqrt(d),mn,mx,n}'; }

echo "### boot check (each variant must run a command) ###"
for v in "${VARIANTS[@]}"; do
  swap "$v"; o=$(AGENT_VM_PROFILE=1 "$AGENTVM" shell echo OK_$v 2>&1)
  echo "$o" | grep -q "OK_$v" && echo "  $v: boots OK" || { echo "  $v: *** FAILED ***"; echo "$o"|tail -3; }
done

declare -A S
echo; echo "### interleaved, 1 GiB / 2 vCPU, $ROUNDS rounds, drop_caches per round ###"
for c in $(seq 1 "$ROUNDS"); do
  sync; echo 3 > /proc/sys/vm/drop_caches 2>/dev/null
  for v in "${VARIANTS[@]}"; do swap "$v"; S[$v]="${S[$v]:-} $(create 1)"; done
  echo "  round $c done"
done

declare -A S4
echo; echo "### heavy vs heavy_deferred @ 4 GiB, 6 rounds (does deferred help at more RAM?) ###"
for c in $(seq 1 6); do
  sync; echo 3 > /proc/sys/vm/drop_caches 2>/dev/null
  for v in heavy heavy_deferred; do swap "$v"; S4[$v]="${S4[$v]:-} $(create 4)"; done
done

echo; echo "================= RESULTS: create wall-seconds @1GiB ================="
for v in "${VARIANTS[@]}"; do printf "%-16s %s\n" "$v" "$(mstat "${S[$v]}")"; done
echo "------- raw @1GiB -------"
for v in "${VARIANTS[@]}"; do printf "%-16s%s\n" "$v" "${S[$v]}"; done
echo; echo "================= @4GiB heavy vs deferred ================="
for v in heavy heavy_deferred; do printf "%-16s %s\n" "$v" "$(mstat "${S4[$v]}")"; done
swap heavy; echo "(restored heavy .so)"
