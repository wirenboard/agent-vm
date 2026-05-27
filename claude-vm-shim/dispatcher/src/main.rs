// claude-vm-dispatcher
//
// Invoked by libclaude-vm-shim.so when the daemon spawns a `--bg-spare` worker.
// Argv: [dispatcher_path, original_claude_path, "--bg-spare", <claim-sock>, ...]
//
// Phase 2: boot an agent-vm with the in-VM claude as the bg-spare, plus a
// guest-side bridge that connects to a host-side TCP listener (via the
// sandbox gateway IP, which microsandbox rewrites to host loopback). We bind
// the host claim-sock and bidi-relay bytes between it and the TCP connection
// from the guest bridge.
//
// Failure to find a usable agent-vm setup or env var
// `CLAUDE_VM_SHIM_PASSTHROUGH=1` falls back to phase-1 behavior (exec the
// host claude verbatim).

use std::ffi::{CString, OsStr, OsString};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const LOG_PATH: &str = "/tmp/claude-vm-dispatcher.log";
const GUEST_SOCK: &str = "/run/claude-vm/bg-spare.sock";

fn debug_log(msg: &str) {
    let pid = std::process::id();
    let envlog = std::env::var("CLAUDE_VM_SHIM_LOG").ok().filter(|p| !p.is_empty());
    for path in envlog.into_iter().chain(std::iter::once(LOG_PATH.to_string())) {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "[dispatcher pid {}] {}", pid, msg);
        }
    }
}

fn passthrough_local_claude(original: PathBuf, forwarded: Vec<OsString>) -> ! {
    debug_log(&format!(
        "PASSTHROUGH exec'ing {} ({} args)",
        original.display(),
        forwarded.len()
    ));
    // Drop shim env vars so the local claude does not re-enter through us.
    std::env::remove_var("LD_PRELOAD");
    std::env::remove_var("CLAUDE_VM_SHIM_DISPATCHER");

    let mut exec_argv: Vec<CString> = Vec::with_capacity(forwarded.len() + 1);
    exec_argv.push(to_cstring(original.clone().into_os_string()));
    for a in forwarded {
        exec_argv.push(to_cstring(a));
    }
    let argv_ptrs: Vec<*const std::ffi::c_char> = exec_argv
        .iter()
        .map(|c| c.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let path_c = to_cstring(original.into_os_string());
    unsafe { libc_execv(path_c.as_ptr(), argv_ptrs.as_ptr()) };
    eprintln!(
        "claude-vm-dispatcher: passthrough execv failed: {}",
        std::io::Error::last_os_error()
    );
    std::process::exit(1);
}

fn to_cstring(os: OsString) -> CString {
    let bytes = os.into_vec();
    let safe: Vec<u8> = bytes.into_iter().take_while(|b| *b != 0).collect();
    CString::new(safe).expect("nul-free after truncation")
}

extern "C" {
    #[link_name = "execv"]
    fn libc_execv(path: *const std::ffi::c_char, argv: *const *const std::ffi::c_char) -> i32;
}

fn main() {
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_PATH)
        .and_then(|mut f| writeln!(f, "[dispatcher pid {}] entered main", std::process::id()));

    let argv_all: Vec<OsString> = std::env::args_os().collect();
    debug_log(&format!(
        "argv={:?}",
        argv_all.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>()
    ));

    if argv_all.len() < 2 {
        eprintln!("claude-vm-dispatcher: missing original claude path");
        std::process::exit(2);
    }

    let original_claude = PathBuf::from(&argv_all[1]);
    let forwarded: Vec<OsString> = argv_all.iter().skip(2).cloned().collect();

    // Passthrough escape hatch.
    if std::env::var("CLAUDE_VM_SHIM_PASSTHROUGH").as_deref() == Ok("1") {
        passthrough_local_claude(original_claude, forwarded);
    }

    // Expect argv[2] == "--bg-spare", argv[3] == claim sock path.
    if forwarded.first().map(|s| s.as_os_str()) != Some(OsStr::new("--bg-spare")) {
        debug_log("argv[2] is not --bg-spare; passing through");
        passthrough_local_claude(original_claude, forwarded);
    }
    let claim_sock_host = match forwarded.get(1) {
        Some(s) => PathBuf::from(s),
        None => {
            debug_log("missing claim sock path; passing through");
            passthrough_local_claude(original_claude, forwarded);
        }
    };
    let extra_args: Vec<OsString> = forwarded.iter().skip(2).cloned().collect();

    if !extra_args.is_empty() {
        debug_log(&format!(
            "warn: {} extra args after claim sock (unused in VM mode): {:?}",
            extra_args.len(),
            extra_args
        ));
    }

    // Run the VM session.
    match run_vm_session(&claim_sock_host) {
        Ok(()) => {
            debug_log("VM session finished cleanly");
            std::process::exit(0);
        }
        Err(e) => {
            debug_log(&format!("VM session failed: {e}; falling back to passthrough"));
            eprintln!("claude-vm-dispatcher: {e}");
            passthrough_local_claude(original_claude, forwarded);
        }
    }
}

fn run_vm_session(claim_sock_host: &std::path::Path) -> std::io::Result<()> {
    debug_log(&format!(
        "VM session begin: claim_sock_host={}",
        claim_sock_host.display()
    ));

    // Bind the host UDS — daemon will connect here believing it's claude --bg-spare.
    let _ = std::fs::remove_file(claim_sock_host);
    if let Some(parent) = claim_sock_host.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let uds_listener = UnixListener::bind(claim_sock_host)?;
    uds_listener.set_nonblocking(false)?;
    debug_log(&format!("bound UDS at {}", claim_sock_host.display()));

    // Bind a host TCP listener on all interfaces. The guest bridge has two
    // possible paths back to us:
    //   - via the sandbox gateway IP — microsandbox would rewrite to 127.0.0.1
    //     but the default policy only allows DNS to the gateway ("host" group).
    //   - via the host's "outward" IP — falls into the "public" group which
    //     the default policy allows on all ports.
    // We expose the second route and pass the host IP to the guest bridge.
    let tcp_listener = TcpListener::bind("0.0.0.0:0")?;
    let tcp_port = tcp_listener.local_addr()?.port();
    let host_ip = detect_host_ip().unwrap_or_else(|| "127.0.0.1".to_string());
    debug_log(&format!("bound TCP at 0.0.0.0:{tcp_port}; guest target = {host_ip}:{tcp_port}"));

    // Locate the bridge binary's host dir, to be mounted into the guest.
    let bridge_host_dir = std::env::var("CLAUDE_VM_SHIM_BRIDGE_DIR")
        .unwrap_or_else(|_| "/opt/claude-vm-shim/bin".to_string());

    // Search standard install locations because the daemon scrubs PATH and
    // a bare "agent-vm" won't resolve via execvp.
    //
    // Prefer a locally-built agent-vm (which has the AGENT_VM_ALLOW_LOCAL_EGRESS
    // policy patch needed for the guest bridge to reach the host TCP listener).
    // The npm-installed `/usr/bin/agent-vm` is a `#!/usr/bin/env node` shim;
    // we point at its platform subpackage ELF instead so env-less exec works.
    let agent_vm = match std::env::var("CLAUDE_VM_SHIM_AGENT_VM_BIN") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            let candidates = [
                "/opt/claude-vm-shim/bin/agent-vm",
                "/usr/lib/node_modules/@wirenboard/agent-vm/node_modules/@wirenboard/agent-vm-linux-x64/bin/agent-vm",
                "/usr/local/lib/node_modules/@wirenboard/agent-vm/node_modules/@wirenboard/agent-vm-linux-x64/bin/agent-vm",
                "/usr/bin/agent-vm",
                "/usr/local/bin/agent-vm",
            ];
            candidates
                .iter()
                .find(|p| std::path::Path::new(p).is_file())
                .map(|p| p.to_string())
                .unwrap_or_else(|| "agent-vm".to_string())
        }
    };
    debug_log(&format!("using agent-vm binary: {agent_vm}"));

    // bash payload running inside the VM. The bridge binary is staged in the
    // project dir, which agent-vm binds at /workspace under tmpfs paths.
    //   1. ensure /run/claude-vm exists
    //   2. start the bridge in background (waits for the sock, then opens TCP)
    //   3. exec the in-VM claude as the bg-spare on the same UDS path
    let bash_payload = format!(
        "set -e; mkdir -p /run/claude-vm; \
         /workspace/claude-vm-bridge --uds {gsock} --host {host_ip} --host-port {port} & \
         BRIDGE_PID=$!; \
         trap 'kill $BRIDGE_PID 2>/dev/null; rm -f {gsock}' EXIT; \
         exec claude --bg-spare {gsock}",
        gsock = GUEST_SOCK,
        port = tcp_port,
        host_ip = host_ip,
    );

    // The daemon spawns us with cwd=/tmp/cc-daemon-0/<id>/spare, which agent-vm
    // would try to mirror inside the VM — that fails because /tmp tmpfs-mount
    // ate any baked workdir. Force a per-dispatcher cwd so agent-vm takes the
    // tmpfs fallback (`mounting at /workspace instead`) and gives each VM a
    // unique state dir (`agent-vm` derives sandbox name from the cwd path).
    let workdir_root = std::env::var("CLAUDE_VM_SHIM_WORKDIR_ROOT")
        .unwrap_or_else(|_| "/tmp/claude-vm-shim-work".to_string());
    let vm_workdir = format!("{workdir_root}/{}", std::process::id());
    let _ = std::fs::create_dir_all(&vm_workdir);

    // Stage the bridge binary inside the workdir so it appears in the VM at
    // /workspace/claude-vm-bridge — avoids spending an extra virtio-fs slot
    // on a separate --mount (the IRQ pool is tight, esp. under nested virt).
    let bridge_src = std::path::Path::new(&bridge_host_dir).join("claude-vm-bridge");
    let bridge_dst = std::path::Path::new(&vm_workdir).join("claude-vm-bridge");
    let _ = std::fs::remove_file(&bridge_dst);
    std::fs::copy(&bridge_src, &bridge_dst).map_err(|e| {
        std::io::Error::other(format!(
            "stage bridge {} -> {}: {e}",
            bridge_src.display(),
            bridge_dst.display()
        ))
    })?;
    // chmod +x just in case the copy stripped it.
    use std::os::unix::fs::PermissionsExt;
    if let Ok(mut p) = std::fs::metadata(&bridge_dst).map(|m| m.permissions()) {
        p.set_mode(0o755);
        let _ = std::fs::set_permissions(&bridge_dst, p);
    }

    // Capture agent-vm stderr to a per-session log file (stdout stays inherited
    // because the bg-spare PTY needs to flow through it).
    let vm_log_path = format!(
        "/tmp/claude-vm-dispatcher.vm-{}.log",
        std::process::id()
    );
    let vm_log = std::fs::File::create(&vm_log_path)
        .map_err(|e| std::io::Error::other(format!("create vm log: {e}")))?;

    debug_log(&format!(
        "spawning agent-vm shell --no-git (cwd={vm_workdir}, stderr→{vm_log_path}) -- bash -c <{}b payload>",
        bash_payload.len()
    ));

    // The locally-built agent-vm doesn't ship a sibling msb / libkrunfw, so
    // point it at the npm-installed bundle (which has both).
    let msb_path = std::env::var("CLAUDE_VM_SHIM_MSB_PATH").unwrap_or_else(|_| {
        "/usr/lib/node_modules/@wirenboard/agent-vm/node_modules/@wirenboard/agent-vm-linux-x64/bin/msb".to_string()
    });

    let mut child: Child = Command::new(&agent_vm)
        // Open the host's reachable TCP port to the guest. agent-vm's patched
        // network policy needs this flag to switch from public-only to
        // non-local egress (allows the guest to TCP-connect to the host's
        // private IP).
        .env("AGENT_VM_ALLOW_LOCAL_EGRESS", "1")
        .env("MSB_PATH", &msb_path)
        .arg("shell")
        // No git creds needed for the bg-spare worker; frees a virtio slot.
        .arg("--no-git")
        .current_dir(&vm_workdir)
        // Inherit stdio (the PTY slave handed to us by --bg-pty-host).
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::from(vm_log))
        // The in-VM agent command:
        .arg("--")
        .arg("bash")
        .arg("-c")
        .arg(&bash_payload)
        .spawn()
        .map_err(|e| std::io::Error::other(format!("spawn agent-vm: {e}")))?;

    debug_log(&format!("agent-vm child pid={}", child.id()));

    // Now accept on both listeners — but we can't block forever; the daemon
    // and bridge each connect on their own timeline. Use a brief deadline.
    let accept_timeout = Duration::from_secs(120);
    debug_log("waiting for daemon UDS connect + guest TCP connect");

    let uds_stream = accept_uds_with_timeout(&uds_listener, accept_timeout)?;
    debug_log("daemon UDS connect accepted");

    let (tcp_stream, peer) = accept_tcp_with_timeout(&tcp_listener, accept_timeout)?;
    debug_log(&format!("guest TCP connect accepted from {peer}"));

    // Read the claim frame (single JSON line + "\n", followed by EOF on the
    // daemon's write half) from UDS, translate host paths into in-VM
    // equivalents, then write the translated frame to TCP.
    let host_project = std::env::var("PWD").unwrap_or_else(|_| String::from("/"));
    let translated = translate_claim_frame_from_uds(uds_stream.try_clone()?, &host_project)?;
    {
        use std::io::Write;
        let mut tcp_writer = tcp_stream.try_clone()?;
        tcp_writer.write_all(&translated)?;
        let _ = shutdown_write(tcp_writer.as_raw_fd());
        debug_log(&format!("forwarded translated claim frame ({} bytes) → TCP", translated.len()));
        // Optional dump of the translated bytes for debugging.
        if let Ok(p) = std::env::var("CLAUDE_VM_SHIM_DUMP_BYTES") {
            if !p.is_empty() {
                let fname = format!("{p}.translated.pid{}", std::process::id());
                let _ = std::fs::write(&fname, &translated);
            }
        }
    }

    // After the claim, bg-spare may reply with bytes (or just close). Mirror
    // any TCP→UDS traffic; the UDS→TCP direction is already drained.
    let stopped = Arc::new(AtomicBool::new(false));
    let stopped_b = stopped.clone();
    let h_tcp_to_uds = thread::spawn(move || {
        copy_until_eof(tcp_stream, uds_stream, "tcp→uds");
        stopped_b.store(true, Ordering::SeqCst);
    });

    let _ = h_tcp_to_uds.join();
    debug_log("bidi-relay halted");

    let _ = std::fs::remove_file(claim_sock_host);

    // Wait for the agent-vm child to exit naturally.
    let status = child
        .wait()
        .map_err(|e| std::io::Error::other(format!("wait for agent-vm: {e}")))?;
    debug_log(&format!("agent-vm exited: {status:?}"));

    Ok(())
}

fn accept_uds_with_timeout(
    listener: &UnixListener,
    timeout: Duration,
) -> std::io::Result<UnixStream> {
    // UnixListener::accept doesn't support a per-call timeout; use a poll loop.
    listener.set_nonblocking(true)?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((s, _)) => {
                s.set_nonblocking(false)?;
                listener.set_nonblocking(false).ok();
                return Ok(s);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "UDS accept timed out",
                    ));
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

fn accept_tcp_with_timeout(
    listener: &TcpListener,
    timeout: Duration,
) -> std::io::Result<(TcpStream, std::net::SocketAddr)> {
    listener.set_nonblocking(true)?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((s, peer)) => {
                s.set_nonblocking(false)?;
                listener.set_nonblocking(false).ok();
                return Ok((s, peer));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "TCP accept timed out",
                    ));
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

fn copy_until_eof<R, W>(mut from: R, mut to: W, label: &'static str)
where
    R: Read,
    W: Write + AsRawFd,
{
    // Optional payload dump for protocol reverse-engineering. When
    // CLAUDE_VM_SHIM_DUMP_BYTES is set, each direction's payload is appended
    // to `<prefix>.<label>-pid<pid>`.
    let dump_path = std::env::var("CLAUDE_VM_SHIM_DUMP_BYTES")
        .ok()
        .filter(|p| !p.is_empty());
    let mut total: u64 = 0;
    let mut buf = [0u8; 16 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => {
                let _ = shutdown_write(to.as_raw_fd());
                debug_log(&format!("{label}: EOF ({total} bytes)"));
                return;
            }
            Ok(n) => {
                total += n as u64;
                if let Some(p) = &dump_path {
                    let fname = format!("{p}.{label}.pid{}", std::process::id());
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&fname)
                    {
                        let _ = f.write_all(&buf[..n]);
                    }
                }
                if let Err(e) = to.write_all(&buf[..n]) {
                    debug_log(&format!("{label}: write error: {e}"));
                    return;
                }
            }
            Err(e) => {
                debug_log(&format!("{label}: read error: {e}"));
                return;
            }
        }
    }
}

/// Read the daemon's claim frame from the UDS, translate it for the in-VM
/// claude, and return the bytes to forward over TCP.
///
/// Wire format (from the daemon, see `Sbz`/`buildClaimFrame` in claude):
/// a single line `JSON.stringify({cwd, env, argv, sessionId}) + "\n"`, then
/// the daemon half-closes its write side (we'll see EOF on read).
fn translate_claim_frame_from_uds(
    mut uds: UnixStream,
    host_project: &str,
) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut raw = Vec::with_capacity(4096);
    let mut buf = [0u8; 4096];
    loop {
        let n = uds.read(&mut buf)?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        // Guard against truly pathological input.
        if raw.len() > 4 * 1024 * 1024 {
            return Err(std::io::Error::other(
                "claim frame exceeded 4MiB; refusing to translate",
            ));
        }
    }

    // Trim trailing newline so the JSON parser is happy.
    let json_bytes: &[u8] = match raw.last() {
        Some(b'\n') => &raw[..raw.len() - 1],
        _ => &raw,
    };
    let mut frame: serde_json::Value = match serde_json::from_slice(json_bytes) {
        Ok(v) => v,
        Err(e) => {
            debug_log(&format!(
                "claim frame is not JSON ({e}); forwarding verbatim {}b",
                raw.len()
            ));
            return Ok(raw);
        }
    };

    translate_claim_frame_value(&mut frame, host_project);

    let mut out = serde_json::to_vec(&frame)
        .map_err(|e| std::io::Error::other(format!("re-serialize claim frame: {e}")))?;
    out.push(b'\n');
    Ok(out)
}

/// Apply path/env translation to a parsed claim frame in place.
fn translate_claim_frame_value(frame: &mut serde_json::Value, host_project: &str) {
    use serde_json::Value;

    // Determine the in-VM project root. agent-vm uses /workspace when the
    // host path is under a tmpfs prefix (/tmp, /run, /dev/shm); otherwise it
    // mirrors the host path.
    let guest_project = host_to_guest_project_path(host_project);
    debug_log(&format!(
        "claim-frame translate: project {host_project:?} → {guest_project:?}"
    ));

    if let Some(obj) = frame.as_object_mut() {
        // cwd: map if it equals or is under host_project
        if let Some(Value::String(cwd)) = obj.get_mut("cwd") {
            if let Some(mapped) = map_path(cwd, host_project, &guest_project) {
                debug_log(&format!("claim-frame cwd {cwd:?} → {mapped:?}"));
                *cwd = mapped;
            }
        }

        // env: drop host-only / outer-VM / shim vars; rewrite PWD/OLDPWD.
        if let Some(Value::Object(env)) = obj.get_mut("env") {
            let to_drop: Vec<String> = env
                .keys()
                .filter(|k| should_drop_env(k))
                .cloned()
                .collect();
            for k in &to_drop {
                env.remove(k);
            }
            if !to_drop.is_empty() {
                debug_log(&format!("claim-frame stripped {} env vars: {:?}", to_drop.len(), to_drop));
            }
            for key in ["PWD", "OLDPWD"] {
                if let Some(Value::String(v)) = env.get_mut(key) {
                    if let Some(mapped) = map_path(v, host_project, &guest_project) {
                        debug_log(&format!("claim-frame env.{key} {v:?} → {mapped:?}"));
                        *v = mapped;
                    }
                }
            }
        }
    }
}

fn host_to_guest_project_path(host: &str) -> String {
    // Same logic as agent-vm's `guest_path_is_safe`: paths under tmpfs prefixes
    // can't be mirrored.
    const TMPFS_PREFIXES: [&str; 3] = ["/tmp/", "/run/", "/dev/shm/"];
    for p in TMPFS_PREFIXES {
        if host.starts_with(p) || host == p.trim_end_matches('/') {
            return "/workspace".to_string();
        }
    }
    host.to_string()
}

fn map_path(s: &str, host: &str, guest: &str) -> Option<String> {
    if s == host {
        return Some(guest.to_string());
    }
    if let Some(tail) = s.strip_prefix(&format!("{host}/")) {
        return Some(format!("{guest}/{tail}"));
    }
    None
}

/// Env vars that should be stripped from the claim frame before forwarding.
/// These are either host-only paths or would cause recursion/misconfiguration
/// inside the VM.
fn should_drop_env(k: &str) -> bool {
    matches!(
        k,
        // Shim plumbing: don't let it re-enter inside the VM.
        "LD_PRELOAD" | "CLAUDE_VM_SHIM_LOG" | "CLAUDE_VM_SHIM_DISPATCHER"
            | "CLAUDE_VM_SHIM_SO" | "CLAUDE_VM_SHIM_REAL_CLAUDE"
            | "CLAUDE_VM_SHIM_PASSTHROUGH" | "CLAUDE_VM_SHIM_DUMP_BYTES"
            | "CLAUDE_VM_SHIM_AGENT_VM_BIN" | "CLAUDE_VM_SHIM_BRIDGE_DIR"
            | "CLAUDE_VM_SHIM_WORKDIR_ROOT" | "CLAUDE_VM_SHIM_HOST_IP"
            | "CLAUDE_VM_SHIM_MSB_PATH"
            // Outer-VM leakage (microsandbox / libkrun internals).
            | "KRUN_WORKDIR" | "KRUN_ENV" | "KRUN_BOOT_START_NS"
            | "MSB_HOSTNAME" | "MSB_NET" | "MSB_NET_IPV4" | "MSB_NET_IPV6"
            | "MSB_TMPFS" | "MSB_DIR_MOUNTS" | "MSB_BLOCK_ROOT"
            | "MSB_HOST_ALIAS" | "MSB_AGENT_VM_ANTHROPIC_UNUSED"
            | "MSB_AGENT_VM_OPENAI_UNUSED" | "MSB_AGENT_VM_GH_UNUSED"
            | "MSB_AGENT_VM_OPENCODE_OPENAI_UNUSED"
            // The bg-rendezvous sock lives at a host path the in-VM claude
            // can't reach. Stripping it forces the session into a simpler
            // mode; relaying it is future work.
            | "CLAUDE_BG_RENDEZVOUS_SOCK"
    )
        // Bun-injected wrapper var (varies per invocation, host-only).
        || k == "_"
}

/// Find a non-loopback IPv4 address the guest can reach us at.
/// Reads /proc/net/fib_trie and picks the first /32 host-local entry that
/// isn't 127.0.0.1. Simpler than parsing `ip` output and doesn't need the
/// userspace tool.
fn detect_host_ip() -> Option<String> {
    if let Ok(env) = std::env::var("CLAUDE_VM_SHIM_HOST_IP") {
        if !env.is_empty() {
            return Some(env);
        }
    }
    let body = std::fs::read_to_string("/proc/net/fib_trie").ok()?;
    let mut last_addr: Option<String> = None;
    for line in body.lines() {
        let l = line.trim();
        if let Some(stripped) = l.strip_prefix("|-- ") {
            last_addr = Some(stripped.to_string());
        } else if l.contains("host LOCAL") {
            if let Some(addr) = &last_addr {
                if !addr.starts_with("127.") {
                    return Some(addr.clone());
                }
            }
        }
    }
    None
}

/// Half-close write side of an fd (UDS or TCP both honor SHUT_WR).
fn shutdown_write(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    // SAFETY: fd is a valid socket descriptor owned by the caller.
    let r = unsafe { libc::shutdown(fd, libc::SHUT_WR) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
