// claude-vm-dispatcher
//
// Invoked by libclaude-vm-shim.so when the daemon spawns a per-session
// worker for `claude remote-control`. Argv:
//
//   dispatcher  original_claude_path  --print --sdk-url <url>
//               --session-id <cse-id>  --input-format stream-json
//               --output-format stream-json  [--replay-user-messages]
//
// We boot a fresh agent-vm, run the same claude invocation inside it, and
// pipe our inherited stdio through. The cloud session's stream-json
// protocol flows end-to-end through stdin/stdout/stderr; no UDS handshake
// is involved.
//
// Set CLAUDE_VM_SHIM_PASSTHROUGH=1 to short-circuit the VM dispatch and
// exec the host claude verbatim (for A/B comparison).

use std::ffi::{CString, OsStr, OsString};
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const LOG_PATH: &str = "/tmp/claude-vm-dispatcher.log";

fn debug_log(msg: &str) {
    let pid = std::process::id();
    let envlog = std::env::var("CLAUDE_VM_SHIM_LOG").ok().filter(|p| !p.is_empty());
    for path in envlog.into_iter().chain(std::iter::once(LOG_PATH.to_string())) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(f, "[dispatcher pid {pid}] {msg}");
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

    if std::env::var("CLAUDE_VM_SHIM_PASSTHROUGH").as_deref() == Ok("1") {
        passthrough_local_claude(original_claude, forwarded);
    }

    // Expect argv[2] == "--print" and "--sdk-url" appears in forwarded.
    // The shim should already filter; treat anything else as a mistake and
    // fall back to passthrough.
    let looks_right = forwarded.first().map(|s| s.as_os_str()) == Some(OsStr::new("--print"))
        && forwarded.iter().any(|s| s == "--sdk-url");
    if !looks_right {
        debug_log("argv does not look like --print --sdk-url; passing through");
        passthrough_local_claude(original_claude, forwarded);
    }

    match run_vm_cloud_session(&forwarded) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            debug_log(&format!("cloud session failed: {e}; falling back to passthrough"));
            eprintln!("claude-vm-dispatcher: {e}");
            passthrough_local_claude(original_claude, forwarded);
        }
    }
}

/// Boot a fresh agent-vm with `agent-vm claude --no-git -- <forwarded>` and
/// propagate the in-VM claude's exit code.
fn run_vm_cloud_session(forwarded: &[OsString]) -> std::io::Result<i32> {
    debug_log(&format!(
        "cloud session begin: {} forwarded args",
        forwarded.len()
    ));

    // Per-dispatcher VM workdir so each session gets its own state dir.
    let workdir_root = std::env::var("CLAUDE_VM_SHIM_WORKDIR_ROOT")
        .unwrap_or_else(|_| "/tmp/claude-vm-shim-work".to_string());
    let vm_workdir = format!("{workdir_root}/{}", std::process::id());
    let _ = std::fs::create_dir_all(&vm_workdir);

    // Drop a `.agent-vm.runtime.sh` hook in the workdir. agent-vm's bash
    // prelude sources this before exec'ing claude, so we can publish
    // `CLAUDE_CODE_*` env vars (notably the per-session JWT in
    // CLAUDE_CODE_SESSION_ACCESS_TOKEN) without putting them on the kernel
    // cmdline — stock x86 COMMAND_LINE_SIZE=2048 truncates and the VM
    // never boots if we add ~11 of these via `agent-vm --env`.
    write_cloud_runtime_hook(&vm_workdir);

    let agent_vm = resolve_agent_vm_binary();
    let msb_path = resolve_msb_path();

    // Capture agent-vm's stderr to a per-session log so we can diagnose
    // boot failures without depending on the rc parent's terminal.
    let vm_log_path = format!(
        "/tmp/claude-vm-dispatcher.vm-{}.log",
        std::process::id()
    );
    let vm_log = std::fs::File::create(&vm_log_path)
        .map_err(|e| std::io::Error::other(format!("create vm log {vm_log_path}: {e}")))?;
    debug_log(&format!("agent-vm stderr → {vm_log_path}"));

    let mut cmd = Command::new(&agent_vm);
    cmd
        // Tell agent-vm to (a) skip the default non-TTY stdin → /dev/null
        // redirect and (b) wire stdin through the streaming exec API.
        // Without this the in-guest claude can't read user messages from
        // the cloud session.
        .env("AGENT_VM_FORWARD_STDIN", "1")
        .arg("claude")
        .arg("--no-git")
        .arg("--memory").arg("1")
        .arg("--cpus").arg("1")
        .current_dir(&vm_workdir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::from(vm_log))
        .arg("--");
    // Pass MSB_PATH only when we actually resolved it — let agent-vm use
    // its own discovery ladder otherwise.
    if let Some(p) = msb_path.as_deref() {
        cmd.env("MSB_PATH", p);
    }
    for a in forwarded {
        cmd.arg(a);
    }
    debug_log(&format!(
        "spawning {agent_vm} claude --no-git -- {:?} (cwd={vm_workdir})",
        forwarded
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    ));

    let mut child = cmd
        .spawn()
        .map_err(|e| std::io::Error::other(format!("spawn agent-vm: {e}")))?;
    debug_log(&format!("agent-vm child pid={}", child.id()));

    let status = child
        .wait()
        .map_err(|e| std::io::Error::other(format!("wait for agent-vm: {e}")))?;
    let code = status.code().unwrap_or(1);
    debug_log(&format!("cloud session exit: {status:?} → code={code}"));
    Ok(code)
}

/// Write a `.agent-vm.runtime.sh` hook into the workdir that exports the
/// `CLAUDE_CODE_*` env vars the rc parent set for this per-session subprocess.
/// agent-vm's bash prelude sources this file before exec'ing claude.
fn dump_env_for_debug() {
    if let Ok(p) = std::env::var("CLAUDE_VM_SHIM_LOG") {
        if !p.is_empty() {
            let dump_path = format!("{p}.env-pid{}", std::process::id());
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&dump_path)
            {
                let mut vars: Vec<_> = std::env::vars().collect();
                vars.sort();
                for (k, v) in vars {
                    let _ = writeln!(f, "{k}={v}");
                }
            }
        }
    }
}

fn write_cloud_runtime_hook(vm_workdir: &str) {
    dump_env_for_debug();
    let path = format!("{vm_workdir}/.agent-vm.runtime.sh");
    let mut script = String::from("#!/bin/sh\n# auto-generated by claude-vm-shim/dispatcher\n");
    for (name, value) in std::env::vars() {
        if !name.starts_with("CLAUDE_CODE_") || value.is_empty() {
            continue;
        }
        // Skip vars that would confuse the in-VM child or are host-specific.
        if matches!(name.as_str(), "CLAUDE_CODE_EXECPATH") {
            continue;
        }
        // Single-quote escape: end quote, escaped quote, reopen quote.
        let escaped = value.replace('\'', "'\\''");
        script.push_str(&format!("export {name}='{escaped}'\n"));
    }
    let _ = std::fs::write(&path, script);
    use std::os::unix::fs::PermissionsExt;
    if let Ok(mut p) = std::fs::metadata(&path).map(|m| m.permissions()) {
        p.set_mode(0o755);
        let _ = std::fs::set_permissions(&path, p);
    }
    debug_log(&format!("wrote cloud runtime hook → {path}"));
}

fn resolve_msb_path() -> Option<String> {
    if let Ok(p) = std::env::var("CLAUDE_VM_SHIM_MSB_PATH") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    let candidates = [
        "/usr/lib/node_modules/@wirenboard/agent-vm/node_modules/@wirenboard/agent-vm-linux-x64/bin/msb",
        "/usr/local/lib/node_modules/@wirenboard/agent-vm/node_modules/@wirenboard/agent-vm-linux-x64/bin/msb",
    ];
    for p in candidates {
        if std::path::Path::new(p).is_file() {
            return Some(p.to_string());
        }
    }
    None
}

fn resolve_agent_vm_binary() -> String {
    if let Ok(p) = std::env::var("CLAUDE_VM_SHIM_AGENT_VM_BIN") {
        if !p.is_empty() {
            return p;
        }
    }
    // Prefer a local patched copy at /opt/claude-vm-shim/bin/agent-vm —
    // the upstream npm one doesn't honor AGENT_VM_FORWARD_STDIN yet, which
    // breaks stream-json input to in-VM claude. Fall back to the
    // npm-installed platform subpackage's ELF (the `/usr/bin/agent-vm`
    // shim is a `#!/usr/bin/env node` wrapper that won't resolve `node`
    // under the daemon's scrubbed PATH).
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
