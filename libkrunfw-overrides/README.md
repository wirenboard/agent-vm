# libkrunfw overrides

Patches applied on top of `containers/libkrunfw` at `LIBKRUNFW_VERSION`
(read from `vendor/microsandbox/crates/utils/lib/lib.rs`) before
building the `.so` that ships in the agent-vm npm package.

The CI `package` job in `.github/workflows/release-npm.yml`:

1. `git clone --branch v$LIBKRUNFW_VERSION --depth 1 https://github.com/containers/libkrunfw`
2. `patch -p1 < libkrunfw-overrides/config-libkrunfw_$ARCH.patch`
3. `make`
4. Copies the resulting `libkrunfw.so.$LIBKRUNFW_VERSION` into
   `npm-dist/agent-vm-$PLATFORM/lib/`.

## Why these patches

Stock libkrunfw ships only the paravirt-guest helpers
(`CONFIG_KVM_GUEST=y`) — the guest kernel can talk to a host KVM but
can't host its own. `agent-vm` needs the in-kernel KVM hypervisor for
Docker-in-Docker / nested KVM use cases, so we flip:

| symbol(s) | reason |
|---|---|
| `CONFIG_KVM=y` + `CONFIG_KVM_INTEL=y` + `CONFIG_KVM_AMD=y` | in-kernel KVM hypervisor for nested KVM use cases |
| `CONFIG_POSIX_MQUEUE=y` | runc/crun default OCI spec always mounts `/dev/mqueue`; without it every container start fails with `mount mqueue: no such device` |
| netfilter + bridge family (`NETFILTER`, `NF_CONNTRACK`, `NF_NAT`, `IP_NF_*`, `IP6_NF_*`, `BRIDGE`, `BRIDGE_NETFILTER`, `VLAN_8021Q`, …) | docker/podman `--network=bridge` (the default) needs iptables and the bridge driver to set up `docker0` + SNAT. Without these the daemon starts but every container start fails on `Table does not exist` |

`olddefconfig` pulls in the rest of each cascade (`KVM_X86`, `KVM_SMM`,
`KVM_HYPERV`, `KVM_VFIO`, `POSIX_MQUEUE_SYSCTL`, `NETFILTER_NETLINK`,
`NF_DEFRAG_IPV4`/`IPV6`, `BRIDGE_IGMP_SNOOPING`, …) automatically.

`devtmpfs` and `DEVTMPFS_MOUNT` are already enabled upstream so
`/dev/kvm` materializes at boot without further config changes.

## Cost of each override

Measured by rebuilding the `.so` from a pristine v5.2.1 checkout with each
hunk added incrementally (Debian 13 toolchain, gcc 14.2.0). Boot time is
15-run mean of a nested `msb run alpine -- /bin/true`, sd ≈ 17–20 ms:

| cumulative change | `bzImage` | `libkrunfw.so` (shipped) | nested-boot mean |
|---|---|---|---|
| baseline (just `+KVM`) | 7,431 KB | 21,300 KB | 1.015 s |
| `+POSIX_MQUEUE` | +12 KB | +64 KB (page padding) | +2 ms |
| `+netfilter + bridge` | +348 KB | +0 (still fits in mqueue's page) | +5 ms |

The shipped `.so` is what affects npm-package size and `agent-vm setup` cold-pull
time; the `bzImage` figure is informational. Boot-time deltas were all within
1σ of baseline noise.
