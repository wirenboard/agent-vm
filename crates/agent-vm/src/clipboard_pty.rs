//! Ctrl+V image bridge: forwards host clipboard *images* into the guest.
//!
//! Why: the guest has no display server, so an agent's native image
//! paste can't work there. Claude Code shells out to `xclip` /
//! `wl-paste`; Codex talks to X11/Wayland directly through the
//! `arboard` crate. Either way, Ctrl+V inside the sandbox finds no
//! clipboard. The Lima-era `claude-vm.sh` solved this with a Python
//! pty wrapper (`clipboard-pty.py`) plus in-guest shims; the
//! microsandbox rewrite dropped it. This module is the same idea in
//! Rust, self-contained in the launcher (no image or SDK change).
//!
//! How: for an interactive launch, `agent-vm <agent>` re-executes
//! itself as a child on a pseudo-terminal and the parent relays bytes
//! between the real terminal and that pty (`relay`). When Ctrl+V shows
//! up in the keyboard stream — legacy `0x16` or the kitty keyboard
//! protocol's `CSI 118 ; <mods> u` — the parent snapshots the host
//! clipboard as PNG into `<state>/clipboard/<pid>/paste-NNNNNN.png`,
//! which the guest sees at `/agent-vm-state/clipboard/<pid>/`. Then,
//! depending on [`PasteMode`]:
//!
//!  * [`PasteMode::ForwardKey`] (claude, opencode, copilot, shell): the
//!    key is forwarded untouched, and the guest-side `xclip` /
//!    `wl-paste` shims (see [`GUEST_SHIM_XCLIP`] / [`GUEST_SHIM_WL_PASTE`],
//!    written by `run.rs` into the same dir and put first on PATH) hand
//!    the newest PNG to the agent as if it came from a real clipboard.
//!  * [`PasteMode::PastePath`] (codex): it never shells out, but it
//!    attaches an image whose *path* is pasted into its composer, so
//!    the key is replaced by a bracketed paste of the guest path.
//!
//! The per-launch directory is removed when the wrapper exits, and
//! stale siblings left by crashed launchers are swept at start.
//!
//! Set `AGENT_VM_NO_CLIPBOARD_BRIDGE=1` to skip the wrapper entirely
//! (the agent then runs exactly as before, straight on the terminal).

use std::io::{self, IsTerminal, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::host_paths::atomic_write;
use crate::session::ProjectSession;

/// Env var the wrapper sets for its child: the **host** path of this
/// launch's clipboard directory. Its presence is also the "I am the
/// child, don't wrap again" marker. `run.rs` reads it to write the
/// guest shims and to prepend the guest-side dir to PATH.
pub const CHILD_ENV: &str = "AGENT_VM_CLIPBOARD_DIR";

/// Opt-out: skip the pty wrapper and the in-guest shims.
pub const DISABLE_ENV: &str = "AGENT_VM_NO_CLIPBOARD_BRIDGE";

/// Where `<state>/clipboard` lands inside the guest (the whole state
/// dir is bind-mounted at `/agent-vm-state`, see `run.rs`).
pub const GUEST_ROOT: &str = "/agent-vm-state/clipboard";

/// Name of the subdirectory (under both the host state dir and
/// `GUEST_ROOT`) holding the per-launch dirs.
const HOST_SUBDIR: &str = "clipboard";

/// What to feed the guest when Ctrl+V arrives and the host clipboard
/// holds an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteMode {
    /// Forward the key unchanged; the guest `xclip`/`wl-paste` shims
    /// serve the PNG (Claude Code and anything else that shells out).
    ForwardKey,
    /// Replace the key with a bracketed paste of the PNG's guest path
    /// (Codex attaches pasted image paths; it can't use the shims).
    PastePath,
}

/// Wrap the current invocation in the Ctrl+V bridge when it makes
/// sense. Returns `Ok(Some(exit_code))` when this process acted as the
/// wrapper (the caller must exit with that code) and `Ok(None)` when
/// the caller should carry on and launch normally — because we *are*
/// the wrapped child, bridging is disabled, or there's no terminal.
///
/// Called from `main` before the tokio runtime starts: the wrapper is
/// a plain blocking relay and must never touch msb.
pub fn maybe_wrap(mode: PasteMode) -> Result<Option<i32>> {
    if std::env::var_os(CHILD_ENV).is_some() || std::env::var_os(DISABLE_ENV).is_some() {
        return Ok(None);
    }
    if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
        // No interactive terminal: attach() won't be used either, and
        // there is nobody to press Ctrl+V.
        return Ok(None);
    }
    let session = match ProjectSession::for_cwd() {
        Ok(s) => s,
        // Let the real launch produce the (same) error message.
        Err(_) => return Ok(None),
    };
    let root = session.state_dir.join(HOST_SUBDIR);
    let dir = root.join(std::process::id().to_string());
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    sweep_stale_siblings(&root, &dir);

    let code = relay(mode, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    code.map(Some)
}

/// Remove `<root>/<pid>` dirs whose launcher is no longer running
/// (crashed or SIGKILLed before its own cleanup). Best-effort.
fn sweep_stale_siblings(root: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep || !path.is_dir() {
            continue;
        }
        let Some(pid) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if !Path::new("/proc").join(pid.to_string()).exists() {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

// ---------------------------------------------------------------------
// The pty relay
// ---------------------------------------------------------------------

static GOT_WINCH: AtomicBool = AtomicBool::new(false);
static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_winch(_: libc::c_int) {
    GOT_WINCH.store(true, Ordering::SeqCst);
}

extern "C" fn on_forward_signal(sig: libc::c_int) {
    PENDING_SIGNAL.store(sig, Ordering::SeqCst);
}

/// Spawn ourselves (same argv, plus [`CHILD_ENV`]) on a fresh pty and
/// pump bytes until the child exits. Returns the child's exit code
/// (128+signal if it died from one), mirroring what the shell would
/// report for the unwrapped process.
fn relay(mode: PasteMode, dir: &Path) -> Result<i32> {
    let (master, slave) =
        openpty().context("allocating a pseudo-terminal for the clipboard bridge")?;

    // The child's pty starts out as an exact copy of the real terminal
    // (modes + window size), so the launcher's pre-attach phase and the
    // SDK's raw-mode dance behave exactly as without the wrapper.
    let orig = tcgetattr(libc::STDIN_FILENO).context("tcgetattr(stdin)")?;
    tcsetattr(slave.as_raw_fd(), &orig).context("tcsetattr(pty)")?;
    copy_winsize(libc::STDIN_FILENO, slave.as_raw_fd());

    let exe = std::env::current_exe().context("std::env::current_exe")?;
    let mut cmd = Command::new(exe);
    cmd.args(std::env::args_os().skip(1));
    cmd.env(CHILD_ENV, dir);
    cmd.stdin(Stdio::from(slave.try_clone().context("dup pty")?));
    cmd.stdout(Stdio::from(slave.try_clone().context("dup pty")?));
    cmd.stderr(Stdio::from(slave));
    // Make the pty the child's controlling terminal: SIGWINCH from our
    // TIOCSWINSZ below, SIGHUP when we go away, job control — all the
    // semantics the launcher had when it sat on the real terminal.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .context("re-executing agent-vm under the clipboard bridge")?;
    // Drop our copies of the slave end now, otherwise the master never
    // reports EIO when the child exits and we'd hang forever.
    drop(cmd);

    unsafe {
        libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t);
        for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
            libc::signal(sig, on_forward_signal as *const () as libc::sighandler_t);
        }
    }

    // Raw mode on the real terminal: every key (Ctrl+C, Ctrl+Z, ...)
    // travels to the child's pty as bytes, whose own line discipline
    // decides what they mean — exactly as if the child sat on the
    // terminal itself.
    let mut raw = orig;
    unsafe { libc::cfmakeraw(&mut raw) };
    tcsetattr(libc::STDIN_FILENO, &raw).context("entering raw mode")?;
    let _restore = scopeguard(move || {
        let _ = tcsetattr(libc::STDIN_FILENO, &orig);
    });

    let child_pid = child.id() as libc::pid_t;
    let guest_dir = format!("{GUEST_ROOT}/{}", std::process::id());
    let mut scanner = KeyScanner::default();
    let mut counter: u32 = 0;
    let mut on_ctrl_v = |out: &mut Vec<u8>, key: &[u8]| {
        let guest_path = snapshot_clipboard(dir, &guest_dir, &mut counter, mode);
        match (mode, guest_path) {
            (PasteMode::PastePath, Some(path)) => {
                out.extend_from_slice(&bracketed_paste(&path));
            }
            _ => out.extend_from_slice(key),
        }
    };

    let mut stdout = io::stdout().lock();
    let mut stdin_open = true;
    let mut buf = [0u8; 4096];
    loop {
        if GOT_WINCH.swap(false, Ordering::SeqCst) {
            // The kernel raises SIGWINCH in the child for us on resize.
            copy_winsize(libc::STDIN_FILENO, master.as_raw_fd());
        }
        let sig = PENDING_SIGNAL.swap(0, Ordering::SeqCst);
        if sig != 0 {
            unsafe { libc::kill(child_pid, sig) };
        }

        let mut fds = [
            libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: if stdin_open { libc::POLLIN } else { 0 },
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 100) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            eprintln!("\r\nagent-vm: clipboard bridge: poll failed: {err}\r");
            break;
        }

        if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match read_fd(master.as_raw_fd(), &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    stdout.write_all(&buf[..n]).ok();
                    stdout.flush().ok();
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                // EIO: the child closed its side (exited). Normal.
                Err(_) => break,
            }
        }

        if stdin_open && fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match read_fd(libc::STDIN_FILENO, &mut buf) {
                Ok(0) => stdin_open = false,
                Ok(n) => {
                    let out = scanner.scan(&buf[..n], &mut on_ctrl_v);
                    if let Err(e) = write_all_fd(master.as_raw_fd(), &out) {
                        eprintln!("\r\nagent-vm: clipboard bridge: {e:#}\r");
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => stdin_open = false,
            }
        }
    }

    // Closing the master hangs up the child's terminal; a no-op when it
    // already exited (the usual case), the equivalent of the user
    // closing the terminal window if we bailed out of the loop early.
    drop(master);
    let status = child.wait().context("waiting for the wrapped agent-vm")?;
    Ok(exit_code_of(status))
}

fn exit_code_of(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(1)
}

fn openpty() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // openpty(3) doesn't set O_CLOEXEC: without this the child (and
    // everything it spawns, msb and the VM included) would inherit the
    // master end, and a dead parent would never hang up the child's
    // terminal.
    for fd in [master, slave] {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    // SAFETY: openpty handed us two fresh, owned descriptors.
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

fn tcgetattr(fd: RawFd) -> io::Result<libc::termios> {
    let mut t = MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, t.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { t.assume_init() })
}

fn tcsetattr(fd: RawFd, t: &libc::termios) -> io::Result<()> {
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn copy_winsize(from: RawFd, to: RawFd) {
    let mut ws = MaybeUninit::<libc::winsize>::uninit();
    unsafe {
        if libc::ioctl(from, libc::TIOCGWINSZ as _, ws.as_mut_ptr()) == 0 {
            libc::ioctl(to, libc::TIOCSWINSZ as _, ws.as_ptr());
        }
    }
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

fn write_all_fd(fd: RawFd, mut data: &[u8]) -> Result<()> {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err).context("writing to the agent's pty");
        }
        data = &data[n as usize..];
    }
    Ok(())
}

/// Minimal drop guard (avoids pulling in the `scopeguard` crate).
fn scopeguard<F: FnMut()>(f: F) -> impl Drop {
    struct Guard<F: FnMut()>(F);
    impl<F: FnMut()> Drop for Guard<F> {
        fn drop(&mut self) {
            (self.0)();
        }
    }
    Guard(f)
}

// ---------------------------------------------------------------------
// Keyboard stream scanning
// ---------------------------------------------------------------------

const ESC: u8 = 0x1b;
const CTRL_V: u8 = 0x16;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Stateful Ctrl+V detector over the raw keyboard byte stream.
///
/// Recognises the legacy `0x16` byte and the kitty keyboard protocol
/// encoding `CSI 118 [:alt] ; <mods> [:event] u` (Ctrl bit set, press
/// or repeat event). Bytes inside a bracketed paste (`CSI 200 ~` ..
/// `CSI 201 ~`) are never interpreted — pasted *text* may legitimately
/// contain a ^V. Everything not recognised is passed through verbatim.
#[derive(Default)]
struct KeyScanner {
    in_paste: bool,
}

impl KeyScanner {
    /// Scan one chunk read from the terminal. Returns the bytes to
    /// forward; `on_ctrl_v(out, key_bytes)` is invoked for each Ctrl+V
    /// and decides what to append in place of the key.
    fn scan(&mut self, data: &[u8], on_ctrl_v: &mut dyn FnMut(&mut Vec<u8>, &[u8])) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut i = 0;
        while i < data.len() {
            if self.in_paste {
                match find(&data[i..], PASTE_END) {
                    Some(j) => {
                        let end = i + j + PASTE_END.len();
                        out.extend_from_slice(&data[i..end]);
                        self.in_paste = false;
                        i = end;
                    }
                    None => {
                        out.extend_from_slice(&data[i..]);
                        break;
                    }
                }
                continue;
            }
            let b = data[i];
            if b == CTRL_V {
                on_ctrl_v(&mut out, &data[i..i + 1]);
                i += 1;
            } else if b == ESC && data[i..].starts_with(PASTE_START) {
                out.extend_from_slice(PASTE_START);
                self.in_paste = true;
                i += PASTE_START.len();
            } else if b == ESC {
                match parse_csi_u(&data[i..]) {
                    Some((len, true)) => {
                        on_ctrl_v(&mut out, &data[i..i + len]);
                        i += len;
                    }
                    Some((len, false)) => {
                        out.extend_from_slice(&data[i..i + len]);
                        i += len;
                    }
                    None => {
                        out.push(b);
                        i += 1;
                    }
                }
            } else {
                out.push(b);
                i += 1;
            }
        }
        out
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Parse a kitty-protocol key report `ESC [ key[:alt[:base]] [; mods[:event]] u`
/// at the start of `s`. Returns `(sequence_len, is_ctrl_v_press)`, or
/// `None` if `s` doesn't start with a complete report of that shape.
fn parse_csi_u(s: &[u8]) -> Option<(usize, bool)> {
    if !s.starts_with(b"\x1b[") {
        return None;
    }
    let mut i = 2;
    let num = |i: &mut usize| -> Option<u32> {
        let start = *i;
        while *i < s.len() && s[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i == start || *i - start > 6 {
            return None;
        }
        std::str::from_utf8(&s[start..*i]).ok()?.parse().ok()
    };
    let key = num(&mut i)?;
    // Optional alternate key codes (shifted key, base-layout key).
    for _ in 0..2 {
        if s.get(i) == Some(&b':') {
            i += 1;
            num(&mut i)?;
        }
    }
    let mut mods = 1;
    let mut event = 1;
    if s.get(i) == Some(&b';') {
        i += 1;
        mods = num(&mut i)?;
        if s.get(i) == Some(&b':') {
            i += 1;
            event = num(&mut i)?;
        }
    }
    if s.get(i) != Some(&b'u') {
        return None;
    }
    i += 1;
    let ctrl = mods >= 1 && ((mods - 1) & 4) != 0;
    let press = event == 1 || event == 2;
    Some((i, key == 118 && ctrl && press))
}

/// Wrap `text` in a bracketed-paste envelope, the form a terminal
/// uses to deliver a paste to an application that enabled mode 2004.
fn bracketed_paste(text: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(text.len() + PASTE_START.len() + PASTE_END.len());
    v.extend_from_slice(PASTE_START);
    v.extend_from_slice(text.as_bytes());
    v.extend_from_slice(PASTE_END);
    v
}

// ---------------------------------------------------------------------
// Host clipboard
// ---------------------------------------------------------------------

/// Read the host clipboard as PNG and store it as the next
/// `paste-NNNNNN.png` in `dir`. Returns the file's guest path, or
/// `None` when the clipboard holds no image (or no tool is available).
///
/// In `ForwardKey` mode the previous snapshots are removed first (and
/// also when there is no image now): the guest shims serve "the newest
/// file", and a stale one would make a Ctrl+V with text on the
/// clipboard paste last week's screenshot. Claude Code copies the
/// bytes into memory as soon as it reads them, so nothing is lost. In
/// `PastePath` mode the files stay: Codex only reads the path when the
/// message is submitted, possibly several pastes later.
fn snapshot_clipboard(
    dir: &Path,
    guest_dir: &str,
    counter: &mut u32,
    mode: PasteMode,
) -> Option<String> {
    let png = read_host_clipboard_png();
    if mode == PasteMode::ForwardKey {
        remove_snapshots(dir);
    }
    let png = png?;
    *counter += 1;
    let name = snapshot_name(*counter);
    if let Err(e) = atomic_write(&dir.join(&name), &png, 0o600) {
        eprintln!(
            "\r\nagent-vm: clipboard bridge: cannot write {}: {e}\r",
            dir.join(&name).display()
        );
        return None;
    }
    Some(format!("{guest_dir}/{name}"))
}

fn snapshot_name(n: u32) -> String {
    format!("paste-{n:06}.png")
}

fn remove_snapshots(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("paste-") && name.ends_with(".png") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Try the clipboard tools of the running session type first (Wayland
/// vs X11), each with a short timeout: `wl-paste` blocks forever when
/// there is no compositor to talk to, and `xclip` when the selection
/// owner is unresponsive.
fn read_host_clipboard_png() -> Option<Vec<u8>> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    let wl: (&str, &[&str]) = ("wl-paste", &["--no-newline", "--type", "image/png"]);
    let xc: (&str, &[&str]) = (
        "xclip",
        &["-selection", "clipboard", "-t", "image/png", "-o"],
    );
    let order: &[(&str, &[&str])] = match (wayland, x11) {
        (true, _) => &[wl, xc],
        (false, true) => &[xc, wl],
        (false, false) => &[wl, xc],
    };
    for (cmd, args) in order {
        if which(cmd).is_none() {
            continue;
        }
        if let Some(bytes) = run_with_timeout(cmd, args, Duration::from_secs(3))
            && bytes.starts_with(PNG_MAGIC)
        {
            return Some(bytes);
        }
    }
    None
}

/// Run `cmd args` with stdin closed and return its stdout on success.
/// Kills the child if it outlives `timeout`.
fn run_with_timeout(cmd: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    // Drain stdout on a helper thread so a large image can't fill the
    // pipe and deadlock against our wait loop.
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        out.read_to_end(&mut v).ok();
        v
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let bytes = reader.join().ok()?;
    match status {
        Some(s) if s.success() && !bytes.is_empty() => Some(bytes),
        _ => None,
    }
}

fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(cmd))
        .find(|p| p.is_file())
}

// ---------------------------------------------------------------------
// Guest shims (written by run.rs, executed inside the VM)
// ---------------------------------------------------------------------

/// Shim for Claude Code's Linux clipboard probes:
///
/// ```text
/// xclip -selection clipboard -t TARGETS -o      # list types
/// xclip -selection clipboard -t image/png -o    # read image
/// xclip -selection clipboard -t text/plain -o   # read text (path check)
/// ```
///
/// Only the image case is served. Text reads fail (exit 1) like on a
/// host without a clipboard tool: the host clipboard's *text* is not
/// bridged. `@DIR@` is replaced with the per-launch guest directory.
pub const GUEST_SHIM_XCLIP: &str = r#"#!/bin/sh
# agent-vm clipboard bridge: serves the host's Ctrl+V image snapshot.
# Written by the agent-vm launcher; see crates/agent-vm/src/clipboard_pty.rs.
DIR='@DIR@'
targets=false; output=false; mime=''
for arg in "$@"; do
  case "$arg" in
    TARGETS) targets=true ;;
    -o|-out) output=true ;;
    image/*|text/*) mime="$arg" ;;
  esac
done
$output || exit 1
latest=''
for f in "$DIR"/paste-*.png; do [ -f "$f" ] && latest="$f"; done
[ -n "$latest" ] || exit 1
if $targets; then echo image/png; exit 0; fi
[ "$mime" = image/png ] || exit 1
exec cat "$latest"
"#;

/// Shim for the Wayland flavour of the same probes:
///
/// ```text
/// wl-paste -l | --list-types
/// wl-paste --type image/png   (or -t image/png)
/// wl-paste [--no-newline]     # text read → exit 1 (not bridged)
/// ```
pub const GUEST_SHIM_WL_PASTE: &str = r#"#!/bin/sh
# agent-vm clipboard bridge: serves the host's Ctrl+V image snapshot.
# Written by the agent-vm launcher; see crates/agent-vm/src/clipboard_pty.rs.
DIR='@DIR@'
list=false; mime=''; want_type=false
for arg in "$@"; do
  if $want_type; then mime="$arg"; want_type=false; continue; fi
  case "$arg" in
    -l|--list-types) list=true ;;
    -t|--type) want_type=true ;;
    --type=*) mime="${arg#--type=}" ;;
  esac
done
latest=''
for f in "$DIR"/paste-*.png; do [ -f "$f" ] && latest="$f"; done
[ -n "$latest" ] || exit 1
if $list; then echo image/png; exit 0; fi
[ "$mime" = image/png ] || exit 1
exec cat "$latest"
"#;

/// Write the guest shims into `<host_dir>/bin` and return the guest
/// path of that bin dir (to prepend to the guest PATH). `host_dir` is
/// the value of [`CHILD_ENV`]; it must live under the state dir so it
/// is visible at `GUEST_ROOT/<pid>` inside the VM.
pub fn write_guest_shims(host_dir: &Path, state_dir: &Path) -> Result<String> {
    let rel = host_dir
        .strip_prefix(state_dir)
        .with_context(|| {
            format!(
                "{CHILD_ENV}={} is not under the state dir {}",
                host_dir.display(),
                state_dir.display()
            )
        })?
        .to_str()
        .context("clipboard dir path is not UTF-8")?;
    let guest_dir = format!("/agent-vm-state/{rel}");
    let bin = host_dir.join("bin");
    std::fs::create_dir_all(&bin).with_context(|| format!("creating {}", bin.display()))?;
    for (name, body) in [
        ("xclip", GUEST_SHIM_XCLIP),
        ("wl-paste", GUEST_SHIM_WL_PASTE),
    ] {
        let script = body.replace("@DIR@", &guest_dir);
        atomic_write(&bin.join(name), script.as_bytes(), 0o755)
            .with_context(|| format!("writing clipboard shim {name}"))?;
    }
    Ok(format!("{guest_dir}/bin"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(scanner: &mut KeyScanner, data: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut hits = Vec::new();
        let out = scanner.scan(data, &mut |out, key| {
            hits.push(key.to_vec());
            out.extend_from_slice(key);
        });
        (out, hits)
    }

    #[test]
    fn plain_bytes_pass_through_untouched() {
        let mut s = KeyScanner::default();
        let (out, hits) = scan_all(&mut s, b"hello\r\x1b[A\x03");
        assert_eq!(out, b"hello\r\x1b[A\x03");
        assert!(hits.is_empty());
    }

    #[test]
    fn legacy_ctrl_v_is_detected_and_forwarded() {
        let mut s = KeyScanner::default();
        let (out, hits) = scan_all(&mut s, b"ab\x16cd");
        assert_eq!(out, b"ab\x16cd");
        assert_eq!(hits, vec![b"\x16".to_vec()]);
    }

    #[test]
    fn kitty_ctrl_v_forms_are_detected() {
        for seq in [
            &b"\x1b[118;5u"[..],
            b"\x1b[118;5:1u",
            b"\x1b[118;5:2u",
            b"\x1b[118:86;6u", // ctrl+shift, alternate (shifted) key code
            b"\x1b[118:86:118;5u",
            b"\x1b[118;7u", // ctrl+alt
        ] {
            let mut s = KeyScanner::default();
            let (out, hits) = scan_all(&mut s, seq);
            assert_eq!(out, seq);
            assert_eq!(
                hits,
                vec![seq.to_vec()],
                "{:?}",
                String::from_utf8_lossy(seq)
            );
        }
    }

    #[test]
    fn kitty_non_matches_are_ignored() {
        for seq in [
            &b"\x1b[118;5:3u"[..], // release event
            b"\x1b[118;1u",        // no modifier
            b"\x1b[118;2u",        // shift only
            b"\x1b[99;5u",         // ctrl+c
            b"\x1b[118;5~",        // wrong terminator
            b"\x1b[118;5",         // truncated
        ] {
            let mut s = KeyScanner::default();
            let (out, hits) = scan_all(&mut s, seq);
            assert_eq!(out, seq);
            assert!(hits.is_empty(), "{:?}", String::from_utf8_lossy(seq));
        }
    }

    #[test]
    fn ctrl_v_inside_bracketed_paste_is_data() {
        let mut s = KeyScanner::default();
        let (out, hits) = scan_all(&mut s, b"\x1b[200~a\x16b");
        assert_eq!(out, b"\x1b[200~a\x16b");
        assert!(hits.is_empty());
        // Paste continues in the next chunk, then ends; a Ctrl+V after
        // the end marker counts again.
        let (out, hits) = scan_all(&mut s, b"\x16\x1b[201~\x16");
        assert_eq!(out, b"\x16\x1b[201~\x16");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn callback_can_replace_the_key() {
        let mut s = KeyScanner::default();
        let out = s.scan(b"x\x16y", &mut |out, _| {
            out.extend_from_slice(&bracketed_paste("/p.png"))
        });
        assert_eq!(out, b"x\x1b[200~/p.png\x1b[201~y");
    }

    #[test]
    fn snapshot_names_sort_in_creation_order() {
        assert_eq!(snapshot_name(1), "paste-000001.png");
        assert!(snapshot_name(9) < snapshot_name(10));
        assert!(snapshot_name(99999) < snapshot_name(100000));
    }

    #[test]
    fn guest_shims_serve_newest_snapshot_and_reject_text() {
        // Exercise the real shim scripts with /bin/sh against a temp dir
        // laid out the way the launcher does it.
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path();
        let host_dir = state.join("clipboard").join("4242");
        std::fs::create_dir_all(&host_dir).unwrap();
        let guest_bin = write_guest_shims(&host_dir, state).unwrap();
        assert_eq!(guest_bin, "/agent-vm-state/clipboard/4242/bin");
        // The scripts embed the *guest* path; rewrite it to the host
        // temp dir so we can run them here.
        let bin = host_dir.join("bin");
        for name in ["xclip", "wl-paste"] {
            let p = bin.join(name);
            let body = std::fs::read_to_string(&p)
                .unwrap()
                .replace("/agent-vm-state/clipboard/4242", host_dir.to_str().unwrap());
            atomic_write(&p, body.as_bytes(), 0o755).unwrap();
        }
        let run = |name: &str, args: &[&str]| {
            let out = Command::new(bin.join(name)).args(args).output().unwrap();
            (out.status.code().unwrap(), out.stdout)
        };

        // Empty dir: nothing to list, nothing to read.
        assert_eq!(
            run("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]).0,
            1
        );
        assert_eq!(run("wl-paste", &["-l"]).0, 1);

        std::fs::write(host_dir.join("paste-000001.png"), b"OLD").unwrap();
        std::fs::write(host_dir.join("paste-000002.png"), b"NEW").unwrap();

        let (code, out) = run("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]);
        assert_eq!((code, out.as_slice()), (0, &b"image/png\n"[..]));
        let (code, out) = run(
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        );
        assert_eq!((code, out.as_slice()), (0, &b"NEW"[..]));
        // Claude also probes bmp and text; neither is bridged.
        assert_eq!(
            run(
                "xclip",
                &["-selection", "clipboard", "-t", "image/bmp", "-o"]
            )
            .0,
            1
        );
        assert_eq!(
            run(
                "xclip",
                &["-selection", "clipboard", "-t", "text/plain", "-o"]
            )
            .0,
            1
        );
        assert_eq!(run("xclip", &["-selection", "clipboard", "-o"]).0, 1);

        let (code, out) = run("wl-paste", &["-l"]);
        assert_eq!((code, out.as_slice()), (0, &b"image/png\n"[..]));
        let (code, out) = run("wl-paste", &["--type", "image/png"]);
        assert_eq!((code, out.as_slice()), (0, &b"NEW"[..]));
        let (code, out) = run("wl-paste", &["-t", "image/png"]);
        assert_eq!((code, out.as_slice()), (0, &b"NEW"[..]));
        assert_eq!(run("wl-paste", &["--type", "image/bmp"]).0, 1);
        assert_eq!(run("wl-paste", &["--no-newline"]).0, 1);
        assert_eq!(run("wl-paste", &[]).0, 1);
    }

    #[test]
    fn write_guest_shims_rejects_dir_outside_state() {
        let tmp = tempfile::tempdir().unwrap();
        let err = write_guest_shims(Path::new("/elsewhere/clipboard/1"), tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("not under the state dir"),
            "{err:#}"
        );
    }
}
