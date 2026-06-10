#!/bin/bash
set -u
cd "$(dirname "$0")"
# wait for the main matrix to finish so we don't thrash CPU
until grep -q "ALL VARIANTS BUILT\|ABORTING" /tmp/variants_build.log 2>/dev/null; do sleep 10; done
SRC=linux-6.12.68; OUT=/tmp/variants
echo "### BUILD heavy_legacy ($(date +%T))"
cp /tmp/cfg_heavy_legacy "$SRC/.config"
( cd "$SRC" && make olddefconfig >/dev/null 2>&1 )
printf "    effective: NETFILTER=%s NF_CONNTRACK=%s NF_NAT=%s IP_NF_IPTABLES=%s NF_TABLES=%s BRIDGE=%s IP6_NF=%s\n" \
  "$(grep -c '^CONFIG_NETFILTER=y' $SRC/.config)" "$(grep -c '^CONFIG_NF_CONNTRACK=y' $SRC/.config)" \
  "$(grep -c '^CONFIG_NF_NAT=y' $SRC/.config)" "$(grep -c '^CONFIG_IP_NF_IPTABLES=y' $SRC/.config)" \
  "$(grep -c '^CONFIG_NF_TABLES=y' $SRC/.config)" "$(grep -c '^CONFIG_BRIDGE=y' $SRC/.config)" \
  "$(grep -c '^CONFIG_IP6_NF_IPTABLES=y' $SRC/.config)"
t0=$(date +%s)
( cd "$SRC" && rm -f .version && make -j14 KBUILD_BUILD_TIMESTAMP="Tue Feb 17 16:15:12 CET 2026" KBUILD_BUILD_USER=root KBUILD_BUILD_HOST=libkrunfw vmlinux ) >"$OUT/build-heavy_legacy.log" 2>&1
rc=$?; t1=$(date +%s)
[ $rc -ne 0 ] && { echo "BUILD FAILED rc=$rc"; tail -5 "$OUT/build-heavy_legacy.log"; exit 1; }
python3 bin2cbundle.py -t vmlinux "$SRC/vmlinux" kernel.c
cc -fPIC -DABI_VERSION=5 -shared -Wl,-soname,libkrunfw.so.5 -o "$OUT/libkrunfw-heavy_legacy.so.5.2.1" kernel.c
strip "$OUT/libkrunfw-heavy_legacy.so.5.2.1"
echo "    -> heavy_legacy built in $((t1-t0))s, $(du -h $OUT/libkrunfw-heavy_legacy.so.5.2.1|cut -f1)"
echo "### HEAVY_LEGACY DONE + ALL BUILDS COMPLETE ($(date +%T))"
