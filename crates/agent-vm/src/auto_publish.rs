//! Lima-style auto-port-forwarding for agent-vm.
//!
//! Polls `/proc/net/tcp{,6}` inside the guest over the agentd
//! channel and diff-drives a per-guest-port host listener. The host
//! listener forwards each accepted connection through a
//! per-connection in-guest python3 tunneller (see [`crate::exec_tunnel`]).
//!
//! Forwards both `127.0.0.1` *and* `0.0.0.0` listeners. Lima's
//! default is loopback-only (because on Lima's host-network model
//! `0.0.0.0` inside the guest is genuinely reachable from the LAN,
//! and silently re-exposing that on host loopback would change the
//! security posture). For us, smoltcp is in-process — both 127.0.0.1
//! and 0.0.0.0 inside the guest are reachable only from the
//! agent-vm process anyway, so forwarding either is the same trust
//! boundary and skipping 0.0.0.0 would just mean "the user has to
//! remember which one their dev server picked."
//!
//! Cancellation: the discovery task aborts itself when reading
//! `/proc/net/tcp` errors repeatedly (sandbox shutting down). On
//! ctrl-C the parent runtime drops the tokio handle and all spawned
//! listeners terminate naturally.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::Result;
use microsandbox::Sandbox;
use tokio::task::JoinHandle;

use crate::exec_tunnel;
use crate::proc_net_tcp::{ListenEntry, parse_listen_v4, parse_listen_v6};

/// Poll interval — matches Lima's default cadence and is short
/// enough that an HTTP server started inside the VM appears on the
/// host within a couple seconds.
const POLL_INTERVAL: Duration = Duration::from_millis(2000);

/// Max consecutive read errors before we give up — sandbox is
/// almost certainly shutting down. 5 × 2s = ~10s grace.
const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// Spawn the auto-port-forwarding background task.
pub fn spawn(sandbox: Sandbox) {
    tokio::spawn(async move {
        if let Err(e) = run(sandbox).await {
            tracing::debug!(?e, "auto-publish loop exited");
        }
    });
}

async fn run(sandbox: Sandbox) -> Result<()> {
    let mut active: BTreeMap<u16, ForwardedPort> = BTreeMap::new();
    let mut consecutive_errors = 0u32;
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;

        let tcp4 = match sandbox.fs().read_to_string("/proc/net/tcp").await {
            Ok(s) => s,
            Err(e) => {
                consecutive_errors += 1;
                tracing::debug!(?e, errors = consecutive_errors, "read /proc/net/tcp failed");
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    return Ok(());
                }
                continue;
            }
        };
        let tcp6 = sandbox
            .fs()
            .read_to_string("/proc/net/tcp6")
            .await
            .unwrap_or_default();
        consecutive_errors = 0;

        let wanted: std::collections::BTreeSet<u16> = parse_listen_v4(&tcp4)
            .into_iter()
            .chain(parse_listen_v6(&tcp6))
            .filter(|e| should_forward(*e))
            .map(|e| e.port)
            .collect();

        // ADD: ports newly seen in wanted but not in active.
        let new_ports: Vec<u16> = wanted
            .iter()
            .copied()
            .filter(|p| !active.contains_key(p))
            .collect();
        for port in new_ports {
            match bind_host_for(port).await {
                Ok((host_port, listener)) => {
                    let task = exec_tunnel::spawn_listener(
                        listener,
                        sandbox.clone(),
                        "127.0.0.1".to_string(),
                        port,
                    );
                    eprintln!(
                        "==> auto-publish: guest :{port} → host 127.0.0.1:{host_port}"
                    );
                    active.insert(port, ForwardedPort { host_port, task });
                }
                Err(e) => {
                    tracing::warn!(guest_port = port, ?e, "failed to bind any host port");
                }
            }
        }

        // REMOVE: previously-active ports that disappeared from wanted.
        let stale: Vec<u16> = active
            .keys()
            .copied()
            .filter(|p| !wanted.contains(p))
            .collect();
        for port in stale {
            if let Some(fp) = active.remove(&port) {
                fp.task.abort();
                eprintln!(
                    "==> auto-publish: guest :{port} closed (released host 127.0.0.1:{})",
                    fp.host_port
                );
            }
        }
    }
}

struct ForwardedPort {
    host_port: u16,
    task: JoinHandle<()>,
}

/// Decide whether to auto-forward this listener. Wildcard *and*
/// loopback both qualify — see the module-level comment for why
/// 0.0.0.0 is safe to forward in our in-process-smoltcp setup
/// (unlike Lima).
fn should_forward(entry: ListenEntry) -> bool {
    match entry.addr {
        IpAddr::V4(a) => a.is_unspecified() || a.is_loopback(),
        IpAddr::V6(a) => a.is_unspecified() || a.is_loopback(),
    }
}

/// Try to bind `127.0.0.1:guest_port` first (so the host port
/// mirrors the guest port — Lima's behavior); if that's taken,
/// fall back to an OS-assigned ephemeral port.
async fn bind_host_for(guest_port: u16) -> Result<(u16, tokio::net::TcpListener)> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), guest_port);
    if let Ok(l) = tokio::net::TcpListener::bind(addr).await {
        let p = l.local_addr()?.port();
        return Ok((p, l));
    }
    // Ephemeral.
    let l = tokio::net::TcpListener::bind(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
    ))
    .await?;
    let p = l.local_addr()?.port();
    Ok((p, l))
}
