# agent-vm launch profiling — findings

Host: AMD EPYC, 16 vCPU, nested virt (`/dev/kvm`, `kvm_amd.nested=1`). Image cached.
All headline numbers are **wall-clock**, from interleaved A/Bs (host drift cancels),
`drop_caches` between rounds where noted. Measured with `AGENT_VM_PROFILE=1` plus
sub-timers added to `run.rs` (pre-boot phases + build/spawn+boot+relay) and the
runtime's own `runtime.log` wall timestamps.

## TL;DR

A launch in a GitHub-remote repo broke down (default 2 vCPU / 2 GiB) as:

```
pre: session+reap ............... 0.3 ms
pre: update-check (ghcr HEAD) ... 886 ms   ← notify_if_update_available (banner only)
pre: repo-detect ................   2 ms
pre: secrets (gh auth token) ....  40 ms
pre: gh api user ................ 1290 ms   ← v0.1.14 author identity  ← THE regression
pre: TOTAL pre-boot ............. 2220 ms   ← LARGER than the actual VM boot
create (guest kernel boot) ..... 1500 ms
run (incl. chrome certutil) ....  270 ms
stop / remove ..................   50 ms
TOTAL ~4.1 s
```

**~2.2s of the launch happens before the kernel even starts, and it's two uncached
blocking GitHub/registry network round-trips.** The original `[profile] create` timer
started *after* all of this, which is why earlier profiling missed it entirely.

## The regression: `gh api user` (pre-boot), not the kernel

`discover_host_git_identity()` (added in commit `0c3bb51`, **v0.1.14** — "bake host
gh/git identity into the guest gitconfig", the exact regression window) calls
**`gh api user` first**, an HTTPS round-trip to api.github.com, falling back to the
instant local gitconfig only if it fails. No cache. Measured ~1.26–1.31s here, every
launch. Adding ~1.29s to a ~1.3s baseline ≈ the reported 2.5s.

The ghcr.io **update-check** (`notify_if_update_available`, commit `bfab9d3`) is a second
per-launch blocking HEAD (~0.89s) purely to print a "newer image available" banner.

### Fixes (implemented on this branch, measured)

1. **Cache the resolved git identity** (`secrets.rs`, 24h TTL, validated strings only,
   never tokens; preserves the canonical `gh` identity incl. `gh_login`). Pays the
   `gh api user` cost once per day instead of every launch.
2. **Make the update-check non-blocking** (`run.rs`): spawn it concurrently with boot
   instead of awaiting it. Banner still prints (during boot); never delays launch.

| | pre-boot | total wall |
|---|---|---|
| before | ~2220 ms | ~4100 ms |
| after, run 1 (cold identity cache) | ~1300 ms | ~3280 ms |
| **after, warm cache** | **~32 ms** | **~1900 ms** |

`gh api user` 1290ms → 22µs; update-check 886ms → ~0. **~2.19s off every launch after
the first.** No downside, no kernel rebuild. This alone undoes the regression.

## Secondary: the ~1.5s guest-kernel boot floor

`create` ≈ guest kernel boot (`build()` is ~3µs; `entering VM → agentd core.ready`).
The console (`hvc0`) attaches ~1.3s in, so early boot isn't visible in `kernel.log`;
the A/B below is the real attribution. Five+1 kernels built from one tree
(configs grepped, not assumed), 10 interleaved rounds @1 GiB, `drop_caches` per round:

| kernel | config | create mean ± sd | Δ |
|---|---|---|---|
| stock | upstream libkrunfw (no KVM, no netfilter) | 1.488 ± 0.10 s | — |
| stock+KVM | + CONFIG_KVM/KVM_INTEL/KVM_AMD | 1.587 ± 0.15 s | **+99 ms (KVM)** |
| heavy_nonf | KVM, netfilter off (+mqueue) | 1.615 ± 0.12 s | ≈ stock+KVM ✓ |
| heavy_legacy | + conntrack/NAT/iptables-legacy/bridge | 1.677 ± 0.03 s | **+62 ms (conntrack)** |
| heavy (current) | + full nf_tables/XT/IPv6/VLAN | 1.680 ± 0.03 s | +3 ms (nf_tables ≈ 0) |
| heavy+deferred | heavy + DEFERRED_STRUCT_PAGE_INIT | 1.632 ± 0.03 s | no help |

**The nested-virt kernel rebuild adds only ~190ms total to boot** (KVM ~100ms,
conntrack/iptables ~62ms, nf_tables ~3ms). So it's a real but *minor* secondary cost:

- **KVM (~100ms)** is *required* for nested virt — irremovable.
- **conntrack/iptables-legacy (~62ms)** is the unavoidable cost of docker bridge+SNAT
  (the conntrack hashtable auto-sizes from RAM). With `CONFIG_MODULES is not set` +
  `nomodule`, it can't be made a module — it's built-in or absent. Drop it only if you
  don't need docker networking by default.
- **nf_tables/XT/IPv6/VLAN (~3ms)** — droppable, but saves essentially nothing. Not
  worth the docker-iptables-nft→legacy fallback risk.

### Two levers that do NOT work (verified, don't pursue)

- **`CONFIG_DEFERRED_STRUCT_PAGE_INIT=y`**: no help at 1 GiB, slightly *worse* at 4 GiB.
  Its `defer_init()` heuristic only defers past a 128 MB section threshold after low
  zones init; a 1–4 GiB single-node guest has nothing to defer (it targets TB-scale RAM).
- **`split_irqchip`**: ~515ms swing in the runtime's `boot_time_ms` metric but **zero
  wall-clock effect** (1.91s vs 1.88s create). `boot_time_ms` excludes ~0.9s of early
  boot and is a misleading proxy — rank kernels on wall-clock `create` only. vCPU count:
  also negligible (1/2/4 ≈ 1.84/1.84/1.89s).

## Other real levers

- **Guest memory** (real wall-clock, but EPT/page-materialization under nested virt, not
  struct-page init): create ≈ 1.49s @1G / 1.68–1.92s @2G / 2.9s @4G. Lower the default
  (`AGENT_VM_MEMORY_GIB`, currently 2) for sessions that don't need 2 GiB (~0.2s+).
- **Chrome-MCP CA `certutil` (~270ms)** in the `run` phase — **fixed on this branch.**
  `run.rs` used to run `sudo -u chrome certutil -A …` synchronously in the in-guest
  prelude before exec'ing the agent, on *every* launch. But chromium ignores the system
  CA bundle (which `update-ca-certificates` already populates at boot) and only honors
  its per-user NSS DB, so the CA must be imported there or chrome-devtools-mcp fails
  every HTTPS page with `ERR_CERT_AUTHORITY_INVALID`. The cert can't be baked into the
  shared image (the CA is generated per-install on the host); the import repeated every
  launch only because the NSS DB lives in the ephemeral rootfs. **Fix:** moved the import
  into the in-image `agent-vm-chrome-mcp` wrapper (`images/Dockerfile`) — it runs once at
  MCP startup, as the `chrome` user that owns the DB, off the launch critical path and
  skipped entirely when chrome is unused. Measured: `run` phase 310ms → ~38ms.
  **Coupling:** the `run.rs` and Dockerfile changes must ship together — until the
  template image is rebuilt with the new wrapper, dropping the prelude import would leave
  chrome MCP without the CA.

## Recommended order (by impact × safety)

1. **Cache the git identity** + **non-blocking update-check** — ~2.19s, implemented here,
   zero downside, no rebuild. This *is* the regression fix.
2. **Chrome-MCP certutil moved into the image wrapper** — ~270ms off the `run` phase,
   implemented on this branch (run.rs + images/Dockerfile; ship together).
3. **Lower default guest memory** if 2 GiB is more than agents need — ~0.2s+.
4. Kernel: leave it. The ~190ms it adds is mostly required (KVM, conntrack). Do **not**
   enable deferred-page-init or chase split_irqchip. Optionally drop nf_tables/IPv6 NF to
   shave ~3ms only if you also accept iptables-legacy.

## Reproduce

```bash
cd <github-remote repo>
AGENT_VM_PROFILE=1 agent-vm shell true     # prints pre-boot phases + create/run/stop
# pre-boot network cost, in isolation:
time gh api user >/dev/null ; time curl -sI https://ghcr.io/v2/wirenboard/agent-vm-template/manifests/latest
# kernel A/B: build variants from libkrunfw-src (one tree, grep each .config), swap the
# .so next to msb, measure interleaved with drop_caches — see measure_variants.sh.
```
