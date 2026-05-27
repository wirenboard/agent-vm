// claude-vm-bridge — guest-side relay between an in-VM Unix socket and a host
// TCP endpoint reached via the sandbox gateway IP (which microsandbox rewrites
// to host loopback).
//
// Usage (inside the VM):
//
//   claude-vm-bridge \
//       --uds /run/claude/bg-spare.sock \
//       --host-port 9000 \
//       [--gateway 172.16.0.1]
//
// If --gateway is omitted, the bridge reads `ip route show default` to find
// the gateway IP. The bridge polls until the UDS exists (claude --bg-spare
// creates it shortly after VM boot), then opens TCP to <gateway>:<host_port>
// and a UDS connection, and bidirectionally forwards bytes until either end
// closes.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const READY_POLL_INTERVAL: Duration = Duration::from_millis(100);
const READY_POLL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct Args {
    uds_path: PathBuf,
    host_port: u16,
    /// Explicit host IP for the TCP target. If unset, falls back to the
    /// default gateway from /proc/net/route.
    host: Option<IpAddr>,
}

fn parse_args() -> Result<Args, String> {
    let mut argv = std::env::args().skip(1);
    let mut uds_path: Option<PathBuf> = None;
    let mut host_port: Option<u16> = None;
    let mut host: Option<IpAddr> = None;

    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--uds" => {
                uds_path = Some(PathBuf::from(argv.next().ok_or("--uds needs a value")?));
            }
            "--host-port" => {
                host_port = Some(
                    argv.next()
                        .ok_or("--host-port needs a value")?
                        .parse::<u16>()
                        .map_err(|e| format!("--host-port: {e}"))?,
                );
            }
            "--host" | "--gateway" => {
                host = Some(
                    argv.next()
                        .ok_or("--host needs a value")?
                        .parse::<IpAddr>()
                        .map_err(|e| format!("--host: {e}"))?,
                );
            }
            "--help" | "-h" => {
                eprintln!("{}", USAGE);
                std::process::exit(0);
            }
            other => return Err(format!("unknown arg: {other}")),
        }
    }

    Ok(Args {
        uds_path: uds_path.ok_or("missing --uds")?,
        host_port: host_port.ok_or("missing --host-port")?,
        host,
    })
}

const USAGE: &str = "\
claude-vm-bridge — relay an in-VM Unix socket to a host TCP port.

Usage:
  claude-vm-bridge --uds <path> --host-port <port> [--gateway <ip>]

Options:
  --uds <path>          Path to the in-VM Unix socket (will wait for it to appear)
  --host-port <port>    Host TCP port to connect to (via sandbox gateway)
  --gateway <ip>        Override gateway IP (defaults to the default route's gateway)
";

fn detect_gateway() -> Result<IpAddr, String> {
    // Read /proc/net/route directly — no external tools required.
    //
    // Format (whitespace-separated, header on line 0, hex little-endian ints):
    //   Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT
    //   eth0  00000000    0102000A 0003 0      0   0      00000000 0   0      0
    //
    // The default route has Destination==00000000 (no mask). Gateway is a
    // little-endian hex u32; e.g. 0102000A == 0x0A000201 == 10.0.2.1.
    let body = std::fs::read_to_string("/proc/net/route")
        .map_err(|e| format!("read /proc/net/route: {e}"))?;
    for (idx, line) in body.lines().enumerate() {
        if idx == 0 {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 3 {
            continue;
        }
        if cols[1] == "00000000" {
            let gw_hex = cols[2];
            let raw = u32::from_str_radix(gw_hex, 16)
                .map_err(|e| format!("parse gateway hex {gw_hex:?}: {e}"))?;
            // Little-endian: low byte is first octet.
            let octets = [
                (raw & 0xff) as u8,
                ((raw >> 8) & 0xff) as u8,
                ((raw >> 16) & 0xff) as u8,
                ((raw >> 24) & 0xff) as u8,
            ];
            return Ok(IpAddr::V4(Ipv4Addr::from(octets)));
        }
    }
    Err("no default route in /proc/net/route".into())
}

fn wait_for_uds(path: &Path, deadline: Instant) -> Result<UnixStream, String> {
    loop {
        match UnixStream::connect(path) {
            Ok(s) => return Ok(s),
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "timed out waiting for UDS {path:?}: last error {e}"
                    ));
                }
                thread::sleep(READY_POLL_INTERVAL);
            }
        }
    }
}

fn bidirectional_copy(uds: UnixStream, tcp: std::net::TcpStream) -> io::Result<()> {
    use std::io::{Read, Write};
    let uds_a = uds.try_clone()?;
    let tcp_a = tcp.try_clone()?;

    // uds_a -> tcp_a (guest reads from claude → forwards out to host)
    let h1 = thread::spawn(move || -> io::Result<()> {
        let mut r = uds_a;
        let mut w = tcp_a;
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                let _ = w.shutdown(std::net::Shutdown::Write);
                return Ok(());
            }
            w.write_all(&buf[..n])?;
        }
    });

    // tcp -> uds (host sends in → forwards to claude)
    let mut r = tcp;
    let mut w = uds;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            let _ = w.shutdown(std::net::Shutdown::Write);
            break;
        }
        w.write_all(&buf[..n])?;
    }
    let _ = h1.join();
    Ok(())
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("claude-vm-bridge: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    let host_ip = match args.host {
        Some(h) => h,
        None => match detect_gateway() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("claude-vm-bridge: detect gateway: {e}");
                std::process::exit(1);
            }
        },
    };

    let host = SocketAddr::new(host_ip, args.host_port);
    eprintln!("claude-vm-bridge: uds={} host={host}", args.uds_path.display());

    let deadline = Instant::now() + READY_POLL_TIMEOUT;
    let uds = match wait_for_uds(&args.uds_path, deadline) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("claude-vm-bridge: {e}");
            std::process::exit(1);
        }
    };

    let tcp = match std::net::TcpStream::connect_timeout(&host, Duration::from_secs(10)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("claude-vm-bridge: connect {host}: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = bidirectional_copy(uds, tcp) {
        eprintln!("claude-vm-bridge: relay ended: {e}");
        std::process::exit(1);
    }
}
