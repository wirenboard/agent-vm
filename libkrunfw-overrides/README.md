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

`olddefconfig` pulls in the rest (`KVM_X86`, `KVM_SMM`, `KVM_HYPERV`,
`KVM_VFIO`, …) automatically.

`devtmpfs` and `DEVTMPFS_MOUNT` are already enabled upstream so
`/dev/kvm` materializes at boot without further config changes.
