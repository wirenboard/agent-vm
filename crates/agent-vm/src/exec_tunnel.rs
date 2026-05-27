//! Host → guest port forwarding via per-connection in-guest tunnellers.
//!
//! Avoids touching microsandbox's `PortPublisher` (which would need a
//! runtime add/remove API and cross-process IPC into the `msb` child).
//! Instead, for each accepted host connection we `exec` a tiny python3
//! script inside the guest that bridges its stdin/stdout to a TCP
//! socket on the guest loopback, then pipe bytes between the host
//! TcpStream and the exec session's stdin/stdout over the existing
//! agentd channel.
//!
//! Trade-offs vs the native smoltcp `--publish` path:
//! - one python3 process per inbound connection (heavy: tens of ms
//!   startup, ~10 MiB RSS each) — fine for dev tunnels, bad for
//!   high-throughput traffic.
//! - bytes traverse: host TcpStream → agent.sock → agentd → python3
//!   stdin → 127.0.0.1:guest_port. Extra hops vs smoltcp's direct
//!   `inbound_relay → tcp::Socket` path.
//!
//! Upside: zero changes to the microsandbox SDK, and the guest's
//! 127.0.0.1 listener is reachable (smoltcp publish dials the guest
//! VLAN IP, so `127.0.0.1`-only services are unreachable that way).

use std::sync::Arc;

use anyhow::{Context, Result};
use microsandbox::Sandbox;
use microsandbox::sandbox::exec::{ExecEvent, ExecSink};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

/// Spawn the accept loop for a pre-bound listener. Each accepted
/// connection is bridged through a freshly-exec'd python3 tunneller
/// inside the guest.
///
/// Returns a `JoinHandle` for the listener loop so the auto-publish
/// discovery loop can `.abort()` it when the guest port disappears.
pub fn spawn_listener(
    listener: TcpListener,
    sandbox: Sandbox,
    guest_host: String,
    guest_port: u16,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(?e, "auto-publish accept failed");
                    continue;
                }
            };
            let sb = sandbox.clone();
            let g_host = guest_host.clone();
            tokio::spawn(async move {
                if let Err(e) = bridge_one(sb, stream, &g_host, guest_port).await {
                    tracing::debug!(?peer, ?e, "auto-publish bridge ended");
                }
            });
        }
    })
}

async fn bridge_one(
    sandbox: Sandbox,
    stream: TcpStream,
    guest_host: &str,
    guest_port: u16,
) -> Result<()> {
    let script = build_tunnel_script(guest_host, guest_port);
    let mut handle = sandbox
        .exec_stream_with("python3", |e| {
            e.args(["-u", "-c", script.as_str()]).stdin_pipe()
        })
        .await
        .context("starting in-guest tunneller")?;
    let stdin_sink = handle
        .take_stdin()
        .context("tunneller stdin pipe missing")?;
    let stdin_sink = Arc::new(Mutex::new(stdin_sink));

    let (mut host_rx, mut host_tx) = stream.into_split();

    // host → guest: read TcpStream, push to python stdin
    let sink_for_writer = stdin_sink.clone();
    let host_to_guest = tokio::spawn(async move {
        let mut buf = [0u8; 16384];
        loop {
            match host_rx.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    // half-close: tell python EOF so it shutdown(SHUT_WR)s
                    let _ = close_sink(&sink_for_writer).await;
                    return;
                }
                Ok(n) => {
                    let sink = sink_for_writer.lock().await;
                    if sink.write(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    // guest → host: drain ExecEvent::Stdout, push to TcpStream
    let mut exited = false;
    while let Some(event) = handle.recv().await {
        match event {
            ExecEvent::Stdout(data) => {
                if host_tx.write_all(&data).await.is_err() {
                    break;
                }
            }
            ExecEvent::Exited { .. } | ExecEvent::Failed(_) => {
                exited = true;
                break;
            }
            ExecEvent::Stderr(data) => {
                tracing::debug!(stderr = %String::from_utf8_lossy(&data), "tunneller stderr");
            }
            _ => {}
        }
    }

    let _ = host_tx.shutdown().await;
    host_to_guest.abort();
    if !exited {
        let _ = handle.kill().await;
    }
    Ok(())
}

async fn close_sink(sink: &Arc<Mutex<ExecSink>>) -> Result<(), microsandbox::MicrosandboxError> {
    sink.lock().await.close().await
}

/// Python3 bidirectional bridge: stdin ↔ socket. Half-close aware so
/// HTTP/1.1 keep-alive clients (and other protocols that hold the
/// read half open after sending a request) don't deadlock.
fn build_tunnel_script(host: &str, port: u16) -> String {
    format!(
        r#"
import socket, sys, threading
s = socket.create_connection(("{host}", {port}))
def s2o():
    try:
        while True:
            d = s.recv(16384)
            if not d:
                break
            sys.stdout.buffer.write(d)
            sys.stdout.buffer.flush()
    finally:
        try:
            sys.stdout.buffer.flush()
        except Exception:
            pass
def i2s():
    try:
        while True:
            d = sys.stdin.buffer.read1(16384)
            if not d:
                break
            try:
                s.sendall(d)
            except Exception:
                break
    finally:
        try:
            s.shutdown(socket.SHUT_WR)
        except Exception:
            pass
t = threading.Thread(target=s2o, daemon=True)
t.start()
i2s()
t.join(timeout=5)
"#
    )
}
