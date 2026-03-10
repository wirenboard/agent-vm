#!/usr/bin/env python3
"""
balloon-daemon.py — QEMU virtio-balloon controller.

Connects to a QEMU QMP socket and manages the virtio-balloon device.

Usage:
  balloon-daemon.py <socket> get
  balloon-daemon.py <socket> set <size>
  balloon-daemon.py <socket> daemon [options]

Arguments:
  socket             Path to the QMP Unix socket (e.g. ~/.lima/vm/qmp.sock)

Commands:
  get                Query and print the current balloon size.
  set <size>         Set the balloon to a fixed target size (e.g. 8G, 4096M).
  daemon [options]   Continuously monitor guest memory pressure and auto-adjust
                     the balloon to reclaim unused memory from the host while
                     keeping enough headroom for the guest.

Daemon options:
  --max-memory SIZE  Upper bound / ceiling for the balloon. The guest will never
                     be given more than this. Default: auto-detected from the
                     current balloon size at startup (i.e. QEMU -m value).
  --min-memory SIZE  Lower bound / floor. The guest will always have at least
                     this much memory. Default: 1G.
  --initial-target SIZE
                     Set balloon to this size before entering the monitor loop.
                     Useful for pre-inflating the balloon at boot. Default: none.
  --headroom PCT     Keep at least this percentage of *used* memory as free
                     headroom.  E.g. 40 means desired = used * 1.4.
                     Default: 40.
  --min-free SIZE    Minimum free memory to maintain. The balloon target will
                     always be at least used + min-free, regardless of headroom
                     percentage. Also serves as the emergency deflate threshold:
                     if available memory drops below this, the balloon grows
                     aggressively (2x step). Default: 1G.
  --step SIZE        Base step size for emergency deflation. Normal adjustments
                     jump directly to the desired target. Default: 256M.
  --interval SECS    How often to poll guest memory stats and re-evaluate the
                     balloon target, in seconds. Default: 5.
  -v, --verbose      Log balloon decisions to stderr.

Algorithm:
  On each cycle the daemon computes:
    used    = cur - avail        (actual app memory usage, excludes cache/balloon)
    desired = max(used * (1 + headroom%), used + min_free)
    desired = clamp(desired, min_memory, max_memory)

  Then:
    - If avail < min_free: emergency GROW by 2x step (guest critically low)
    - If cur < desired - step: GROW — jump balloon to desired immediately
    - If cur > desired + step: SHRINK — jump balloon to desired immediately
  Both grow and shrink are fast (single-step jump to target).

Sizes can be specified as: 8G, 8GiB, 8GB, 4096M, 4096MiB, 4096MB, or raw bytes.
"""

import argparse
import json
import signal
import socket
import sys
import time

GiB = 1 << 30
MiB = 1 << 20


def parse_size(s):
    """Parse human-readable size (e.g. '8G', '512M') to bytes."""
    s = s.strip().upper()
    for suffix, mult in [("GIB", GiB), ("GIG", GiB), ("GB", GiB), ("G", GiB),
                          ("MIB", MiB), ("MB", MiB), ("M", MiB)]:
        if s.endswith(suffix):
            return int(float(s[:-len(suffix)]) * mult)
    return int(s)


def fmt(b):
    """Format bytes as human-readable size."""
    if b >= GiB:
        return f"{b / GiB:.1f}G"
    return f"{b // MiB}M"


class QMP:
    """Minimal QMP (QEMU Machine Protocol) client."""

    def __init__(self, path):
        self.path = path
        self.sock = None
        self.buf = b""

    def connect(self, retries=1, delay=1):
        for i in range(retries):
            try:
                self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                self.sock.settimeout(10)
                self.sock.connect(self.path)
                self._recv()  # greeting
                self._send({"execute": "qmp_capabilities"})
                r = self._recv()
                if "error" in r:
                    raise RuntimeError(r["error"])
                return
            except (ConnectionRefusedError, FileNotFoundError, OSError):
                if self.sock:
                    self.sock.close()
                    self.sock = None
                if i < retries - 1:
                    time.sleep(delay)
                else:
                    raise

    def _send(self, obj):
        self.sock.sendall(json.dumps(obj).encode() + b"\n")

    def _recv(self):
        while True:
            nl = self.buf.find(b"\n")
            if nl >= 0:
                line = self.buf[:nl].strip()
                self.buf = self.buf[nl + 1:]
                if line:
                    return json.loads(line)
                continue
            data = self.sock.recv(4096)
            if not data:
                raise ConnectionError("QMP connection closed")
            self.buf += data

    def cmd(self, execute, **kw):
        msg = {"execute": execute}
        if kw:
            msg["arguments"] = kw
        self._send(msg)
        # Skip async events, wait for command response
        while True:
            r = self._recv()
            if "event" not in r:
                return r

    def close(self):
        if self.sock:
            self.sock.close()
            self.sock = None


# ── Balloon device discovery ─────────────────────────────────────────────────

# QOM paths where the balloon device may appear
BALLOON_PATHS = [
    "/machine/peripheral/balloon0",       # explicit id=balloon0
    "/machine/peripheral-anon/balloon0",  # anonymous device
]


def find_balloon(qmp):
    """Find the balloon device QOM path."""
    for path in BALLOON_PATHS:
        r = qmp.cmd("qom-get", path=path, property="guest-stats-polling-interval")
        if "return" in r:
            return path
    return None


# ── Subcommands ──────────────────────────────────────────────────────────────

def cmd_get(args):
    q = QMP(args.socket)
    q.connect(retries=5)
    r = q.cmd("query-balloon").get("return", {})
    print(fmt(r.get("actual", 0)))
    q.close()


def cmd_set(args):
    target = parse_size(args.size)
    q = QMP(args.socket)
    q.connect(retries=5)
    q.cmd("balloon", value=target)
    print(f"Balloon set to {fmt(target)}")
    q.close()


def _qmp_poll(sock_path, retries=5):
    """Connect, return QMP client. Caller must close after use."""
    q = QMP(sock_path)
    q.connect(retries=retries)
    return q


def cmd_daemon(args):
    min_mem = parse_size(args.min_memory)
    max_mem = parse_size(args.max_memory) if args.max_memory else None
    initial = parse_size(args.initial_target) if args.initial_target else None
    step = parse_size(args.step)
    headroom = args.headroom / 100.0
    min_free = parse_size(args.min_free)
    iv = args.interval

    def log(msg):
        if args.verbose:
            print(msg, file=sys.stderr, flush=True)

    running = True

    def stop(*_):
        nonlocal running
        running = False

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    # Initial setup: determine max memory, find balloon device, enable stats
    q = _qmp_poll(args.socket, retries=30)
    log(f"Connected to {args.socket}")

    # Determine max memory from QEMU's configured RAM (not balloon, which may
    # have been shrunk by a previous daemon run)
    if max_mem is None:
        max_mem = (q.cmd("query-memory-size-summary")
                   .get("return", {}).get("base-memory", 0))
        if not max_mem:
            # Fallback: use current balloon size
            max_mem = q.cmd("query-balloon").get("return", {}).get("actual", 0)
        if not max_mem:
            print("Error: cannot determine VM memory size", file=sys.stderr)
            sys.exit(1)
    log(f"Max memory: {fmt(max_mem)}")

    # Find balloon device and enable stats polling
    bp = find_balloon(q)
    if not bp:
        print("Error: balloon device not found (is -device virtio-balloon-pci enabled?)",
              file=sys.stderr)
        sys.exit(1)

    q.cmd("qom-set", path=bp, property="guest-stats-polling-interval", value=iv)
    log(f"Stats polling on {bp} every {iv}s")

    # Set initial balloon target if requested
    if initial is not None:
        target = max(min_mem, min(max_mem, initial))
        q.cmd("balloon", value=target)
        log(f"Initial target: {fmt(target)}")

    q.close()  # Release QMP for other tools

    # Wait for guest to boot and first stats to be collected
    time.sleep(iv + 1)

    # Main loop: reconnect on each cycle to avoid holding the QMP socket
    while running:
        try:
            q = _qmp_poll(args.socket)
        except (ConnectionRefusedError, FileNotFoundError, OSError):
            log("QMP connection failed, VM may have stopped")
            break

        try:
            # Re-enable stats polling (gets reset on reconnect)
            q.cmd("qom-set", path=bp, property="guest-stats-polling-interval", value=iv)

            guest_stats = q.cmd("qom-get", path=bp, property="guest-stats").get("return", {})
            stats = guest_stats.get("stats", {})
            avail = stats.get("stat-available-memory", -1)
            total = stats.get("stat-total-memory", -1)

            if total <= 0 or avail < 0:
                log("Waiting for guest stats...")
                q.close()
                time.sleep(iv)
                continue

            # Base decisions on actual usage, not ceiling.
            # avail = MemAvailable (excludes reclaimable cache).
            # Use balloon target (cur), not MemTotal, since MemTotal is fixed
            # at boot and includes balloon-claimed pages.
            cur = q.cmd("query-balloon").get("return", {}).get("actual", max_mem)
            used = max(0, cur - avail)
            desired = max(int(used * (1 + headroom)), used + min_free)
            desired = max(min_mem, min(max_mem, desired))

            log(f"used={fmt(used)} avail={fmt(avail)} "
                f"cur={fmt(cur)} desired={fmt(desired)}")

            new = cur
            if avail < min_free and cur < max_mem:
                # Critically low — jump to desired (at minimum +2*step)
                new = max(desired, cur + step * 2)
                new = min(new, max_mem)
                log(f"  CRITICAL: {fmt(cur)} -> {fmt(new)}")
            elif cur < desired - step:
                # Guest needs more memory — jump to desired immediately
                new = min(desired, max_mem)
                log(f"  GROW: {fmt(cur)} -> {fmt(new)}")
            elif cur > desired + step:
                # Excess — reclaim immediately
                new = max(desired, min_mem)
                log(f"  SHRINK: {fmt(cur)} -> {fmt(new)}")

            if new != cur:
                q.cmd("balloon", value=new)

        except ConnectionError:
            log("QMP connection lost")
            q.close()
            break
        except Exception as e:
            log(f"Error: {e}")

        q.close()
        time.sleep(iv)

    log("Stopped")


def main():
    ap = argparse.ArgumentParser(description="QEMU virtio-balloon controller")
    ap.add_argument("socket", help="QMP Unix socket path")
    sp = ap.add_subparsers(dest="command", required=True)

    sp.add_parser("get", help="Query current balloon size")

    ps = sp.add_parser("set", help="Set balloon target")
    ps.add_argument("size", help="Target size (e.g. 8G, 4096M)")

    pd = sp.add_parser("daemon", help="Auto-adjust balloon based on memory pressure")
    pd.add_argument("--max-memory", default=None,
                     help="Maximum memory / ceiling (default: auto-detect from QEMU -m)")
    pd.add_argument("--initial-target", default=None,
                     help="Set balloon to this size before entering monitor loop (e.g. 8G)")
    pd.add_argument("--min-memory", default="1G",
                     help="Minimum balloon size / floor (default: 1G)")
    pd.add_argument("--step", default="256M",
                     help="Adjustment step size (default: 256M)")
    pd.add_argument("--headroom", type=float, default=40,
                     help="Keep this %% of used memory as free headroom (default: 40)")
    pd.add_argument("--min-free", default="1G",
                     help="Emergency deflate threshold (default: 1G)")
    pd.add_argument("--interval", type=int, default=5,
                     help="Stats polling interval in seconds (default: 5)")
    pd.add_argument("-v", "--verbose", action="store_true",
                     help="Log decisions to stderr")

    a = ap.parse_args()
    {"get": cmd_get, "set": cmd_set, "daemon": cmd_daemon}[a.command](a)


if __name__ == "__main__":
    main()
