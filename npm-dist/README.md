# npm-dist

Templates and tooling for distributing `agent-vm` via npm.

## Layout

- `agent-vm/` — the user-facing main package. Tiny JS launcher
  (`bin/agent-vm.js`) that detects `${platform}-${arch}` at runtime
  and `execve`s the prebuilt native binary from the matching
  per-platform subpackage. Declares per-platform subpackages as
  `optionalDependencies` so npm installs only the right one.
- `agent-vm-linux-x64/`, `agent-vm-linux-arm64/` — per-platform
  subpackages. Each ships the prebuilt `bin/agent-vm`, the patched
  `bin/msb`, and `lib/libkrunfw.so.5.2.1`. agent-vm finds `msb` and
  `libkrunfw` via `current_exe()`-relative paths so a user's separate
  microsandbox install never shadows them.
- Future per-platform subpackages: `-darwin-arm64`, `-darwin-x64`,
  `-win32-x64`. Add to the launcher's `PLATFORM_PACKAGES` map and to
  the main package's `optionalDependencies`.

## How releases happen

CI populates each subpackage's `bin/` and `lib/` with freshly
cross-compiled artifacts, rewrites every `package.json` version
field to match the release tag, and runs `npm publish` for each
package. See `.github/workflows/release-npm.yml`.

The OCI image is on a separate cadence (hourly cron) — see
`.github/workflows/build-image.yml`. Binary releases pin the
default image to `ghcr.io/wirenboard/agent-vm-template:latest`; users
override per-launch via `--image` or `AGENT_VM_IMAGE_TAG`.

## Local smoke test

To verify the launcher resolves a subpackage correctly without
publishing, drop a prebuilt binary into a subpackage's `bin/` and
`npm link` it:

    # build the binary
    cargo build --release -p agent-vm
    cargo build --release --manifest-path vendor/microsandbox/Cargo.toml \
        -p microsandbox-cli --bin msb

    cp target/release/agent-vm npm-dist/agent-vm-linux-x64/bin/
    cp vendor/microsandbox/target/release/msb npm-dist/agent-vm-linux-x64/bin/
    cp ~/.microsandbox/lib/libkrunfw.so.5.2.1 npm-dist/agent-vm-linux-x64/lib/

    cd npm-dist/agent-vm-linux-x64 && npm link && cd ..
    cd npm-dist/agent-vm && npm link @wirenboard/agent-vm-linux-x64 && npm link

    agent-vm --help   # should exec target/release/agent-vm
