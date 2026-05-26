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

| symbol | reason |
|---|---|
| `CONFIG_KVM=y` | enable the in-kernel KVM hypervisor |
| `CONFIG_KVM_INTEL=y` | Intel VMX backend |
| `CONFIG_KVM_AMD=y` | AMD SVM backend |
| `CONFIG_POSIX_MQUEUE=y` | runc/crun default OCI spec always mounts `/dev/mqueue`; without this every container start fails with `mount mqueue: no such device` |

`olddefconfig` pulls in the rest (`KVM_X86`, `KVM_SMM`, `KVM_HYPERV`,
`KVM_VFIO`, `POSIX_MQUEUE_SYSCTL`, …) automatically.

`devtmpfs` and `DEVTMPFS_MOUNT` are already enabled upstream so
`/dev/kvm` materializes at boot without further config changes.

## Cost of each override

Measured by rebuilding the `.so` from a pristine v5.2.1 checkout with and
without each hunk (Debian 13 toolchain, gcc 14.2.0):

| change | `bzImage` Δ | stripped `vmlinux` Δ | `libkrunfw.so` Δ | boot Δ |
|---|---|---|---|---|
| `+POSIX_MQUEUE` | +12 KB (+0.17%) | +4 KB (+0.02%) | +64 KB (+0.31%, page padding) | none (15-run mean within noise on nested boot) |
