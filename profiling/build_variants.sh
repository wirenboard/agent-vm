#!/bin/bash
# Build libkrunfw .so variants for a boot-time A/B. One shared tree, serial
# incremental builds. Each .so saved to /tmp/variants/.
set -u
cd "$(dirname "$0")"
SRC=linux-6.12.68
OUT=/tmp/variants
mkdir -p "$OUT"
JOBS=14
KFLAGS=(KBUILD_BUILD_TIMESTAMP="Tue Feb 17 16:15:12 CET 2026" KBUILD_BUILD_USER=root KBUILD_BUILD_HOST=libkrunfw)

build_one() {
  local name="$1" seed="$2"
  echo "===================================================================="
  echo "### BUILD $name  ($(date +%T))"
  cp "$seed" "$SRC/.config" || return 1
  ( cd "$SRC" && make olddefconfig >/dev/null 2>&1 ) || { echo "olddefconfig FAILED"; return 1; }
  # report effective symbols
  printf "    effective: KVM=%s KVM_INTEL=%s KVM_AMD=%s NETFILTER=%s NF_CONNTRACK=%s NF_TABLES=%s BRIDGE=%s POSIX_MQUEUE=%s DEFERRED=%s\n" \
    "$(grep -c '^CONFIG_KVM=y' $SRC/.config)" \
    "$(grep -c '^CONFIG_KVM_INTEL=y' $SRC/.config)" \
    "$(grep -c '^CONFIG_KVM_AMD=y' $SRC/.config)" \
    "$(grep -c '^CONFIG_NETFILTER=y' $SRC/.config)" \
    "$(grep -c '^CONFIG_NF_CONNTRACK=y' $SRC/.config)" \
    "$(grep -c '^CONFIG_NF_TABLES=y' $SRC/.config)" \
    "$(grep -c '^CONFIG_BRIDGE=y' $SRC/.config)" \
    "$(grep -c '^CONFIG_POSIX_MQUEUE=y' $SRC/.config)" \
    "$(grep -c '^CONFIG_DEFERRED_STRUCT_PAGE_INIT=y' $SRC/.config)"
  local t0=$(date +%s)
  ( cd "$SRC" && rm -f .version && make -j$JOBS "${KFLAGS[@]}" vmlinux ) >"$OUT/build-$name.log" 2>&1
  local rc=$?
  local t1=$(date +%s)
  if [ $rc -ne 0 ]; then echo "    BUILD FAILED rc=$rc (see $OUT/build-$name.log)"; tail -5 "$OUT/build-$name.log"; return 1; fi
  echo "    vmlinux built in $((t1-t0))s"
  python3 bin2cbundle.py -t vmlinux "$SRC/vmlinux" kernel.c || { echo "bin2cbundle FAILED"; return 1; }
  cc -fPIC -DABI_VERSION=5 -shared -Wl,-soname,libkrunfw.so.5 -o "$OUT/libkrunfw-$name.so.5.2.1" kernel.c || { echo "link FAILED"; return 1; }
  strip "$OUT/libkrunfw-$name.so.5.2.1"
  echo "    -> $OUT/libkrunfw-$name.so.5.2.1 ($(du -h "$OUT/libkrunfw-$name.so.5.2.1" | cut -f1))  total $((t1-t0))s+bundle"
}

for v in stock stock_kvm heavy_nonf heavy heavy_deferred; do
  build_one "$v" "/tmp/cfg_$v" || { echo "ABORTING at $v"; exit 1; }
done
echo "### ALL VARIANTS BUILT ($(date +%T))"
ls -la "$OUT"/*.so.5.2.1
