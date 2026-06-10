# libkrunfw overrides

Patches applied on top of `containers/libkrunfw` at `LIBKRUNFW_VERSION`
(read from `vendor/microsandbox/crates/utils/lib/lib.rs`) before
building the `.so` that ships in the agent-vm npm package.

The CI `package` job in `.github/workflows/release-npm.yml`:

1. `git clone --branch v$LIBKRUNFW_VERSION --depth 1 https://github.com/containers/libkrunfw`
2. Apply each `*.patch` in this directory.
3. `make`
4. Copies the resulting `libkrunfw.so.$LIBKRUNFW_VERSION` into
   `npm-dist/agent-vm-$PLATFORM/lib/`.

## Patches

### `config-libkrunfw_x86_64.patch` — nested KVM

Stock libkrunfw ships only the paravirt-guest helpers
(`CONFIG_KVM_GUEST=y`) — the guest kernel can talk to a host KVM but
can't host its own. `agent-vm` needs the in-kernel KVM hypervisor for
Docker-in-Docker / nested KVM use cases, so we flip:

| symbol(s) | reason |
|---|---|
| `CONFIG_KVM=y` + `CONFIG_KVM_INTEL=y` + `CONFIG_KVM_AMD=y` | in-kernel KVM hypervisor for nested KVM use cases |
| `CONFIG_POSIX_MQUEUE=y` | runc/crun default OCI spec always mounts `/dev/mqueue`; without it every container start fails with `mount mqueue: no such device` |
| netfilter + bridge family (`NETFILTER`, `NF_CONNTRACK`, `NF_NAT`, `IP_NF_*`, `IP6_NF_*`, `BRIDGE`, `BRIDGE_NETFILTER`, `VLAN_8021Q`, …) | docker/podman `--network=bridge` (the default) needs iptables and the bridge driver to set up `docker0` + SNAT. Without these the daemon starts but every container start fails on `Table does not exist` |
| `nf_tables` family (`NF_TABLES`, `NFT_NAT`, `NFT_MASQ`, `NFT_CT`, `NFT_COMPAT`, …) | debian's default `iptables` alternative is `iptables-nft`, which talks to the kernel via nf_tables instead of ip_tables; without these the user has to flip `update-alternatives --set iptables iptables-legacy` first |

`olddefconfig` pulls in the rest of each cascade (`KVM_X86`, `KVM_SMM`,
`KVM_HYPERV`, `KVM_VFIO`, `POSIX_MQUEUE_SYSCTL`, `NETFILTER_NETLINK`,
`NF_DEFRAG_IPV4`/`IPV6`, `BRIDGE_IGMP_SNOOPING`, …) automatically.

`devtmpfs` and `DEVTMPFS_MOUNT` are already enabled upstream so
`/dev/kvm` materializes at boot without further config changes.

## Kernel-source overrides

In addition to the kbuild `.config` seed above, libkrunfw-overrides/
can ship arch-suffixed source patches (named `<topic>_<arch>.patch`)
that the CI workflow drops into libkrunfw's `patches/0999-overrides-*`
slot for the build to apply.

### `cmdline-size_x86_64.patch` — higher `--mount` cap

x86 Linux statically sizes `boot_command_line[COMMAND_LINE_SIZE]`
(default 2048) and silently truncates anything past that at
`setup_arch()` time. libkrun appends one
`virtio_mmio.device=4K@0xd0XXX000:NN` entry (~36 bytes) per virtio
device, so a baseline boot (rootfs/upper/runtime/network/vsock/
console/etc.) plus ~10 `--mount` entries already pushes the assembled
cmdline past 2048 bytes; the trailing virtio devices then fail to
register and the guest hangs in early boot with `kernel.log` stuck at
0 bytes (the lost devices include virtio-console, so even printk has
nowhere to land).

Bumping `COMMAND_LINE_SIZE` to 16384 leaves room for ~100 user mounts
before re-hitting the cap. libkrun also warns when the assembled
cmdline crosses 2048 bytes, so configurations relying on this patch
flag themselves if run against stock libkrunfw.

## Cost of each override

Measured by rebuilding the `.so` from a pristine v5.2.1 checkout with each
hunk added incrementally (Debian 13 toolchain, gcc 14.2.0). Boot time is
15-run mean of a nested `msb run alpine -- /bin/true`, sd ≈ 17–20 ms:

| cumulative change | `bzImage` | `libkrunfw.so` (shipped) | nested-boot mean |
|---|---|---|---|
| baseline (just `+KVM`) | 7,431 KB | 21,300 KB | 1.015 s |
| `+POSIX_MQUEUE` | +12 KB | +64 KB (page padding) | +2 ms |
| `+netfilter + bridge` | +348 KB | +0 (still fits in mqueue's page) | +5 ms |
| `+nf_tables / NFT_*` | +8 KB | +64 KB (next page) | +3 ms |
| **all overrides** | **+368 KB (+5%)** | **+128 KB (+0.6%)** | **+10 ms (~1%)** |

The shipped `.so` is what affects npm-package size and `agent-vm setup` cold-pull
time; the `bzImage` figure is informational. Boot-time deltas were all within
1σ of baseline noise.
