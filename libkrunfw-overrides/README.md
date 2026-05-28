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

| symbol | reason |
|---|---|
| `CONFIG_KVM=y` | enable the in-kernel KVM hypervisor |
| `CONFIG_KVM_INTEL=y` | Intel VMX backend |
| `CONFIG_KVM_AMD=y` | AMD SVM backend |

`olddefconfig` pulls in the rest (`KVM_X86`, `KVM_SMM`, `KVM_HYPERV`,
`KVM_VFIO`, …) automatically.

`devtmpfs` and `DEVTMPFS_MOUNT` are already enabled upstream so
`/dev/kvm` materializes at boot without further config changes.

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
