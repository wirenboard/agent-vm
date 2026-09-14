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
//! up in the keyboard stream — legacy `0x16`, the kitty keyboard
//! protocol's `CSI 118 ; <mods> u`, or xterm's `modifyOtherKeys`
//! `CSI 27 ; <mods> ; 118 ~` — the parent snapshots the host clipboard
//! as PNG into `<state>/clipboard/<pid>/paste-NNNNNN.png`, which the
//! guest sees at `/agent-vm-state/clipboard/<pid>/`. Then, depending on
//! [`PasteMode`]:
//!
//!  * [`PasteMode::ForwardKey`] (claude, opencode, copilot, shell): the
//!    key is forwarded untouched, and the guest-side `xclip` /
//!    `wl-paste` shims (see [`GUEST_SHIM_XCLIP`] / [`GUEST_SHIM_WL_PASTE`],
//!    written into `<pid>/bin`, which `run.rs` puts first on the guest
//!    PATH) hand the newest PNG to the agent as if it came from a real
//!    clipboard. Snapshots are cleared on the next Ctrl+V (and at
//!    exit): the guest reads within milliseconds, and a background
//!    timer would risk deleting the file mid-read, since Claude probes
//!    the type and reads the bytes in two separate `xclip` calls.
//!  * [`PasteMode::PastePath`] (codex): it never shells out, but it
//!    attaches an image whose *path* is pasted into its composer, so
//!    the key is replaced by a bracketed paste of the guest path (only
//!    while the agent has bracketed paste switched on). Snapshots stay
//!    until exit because Codex reads the file when the message is sent.
//!
//! Security model: the state dir is bind-mounted read-write into the
//! guest, so everything under `<state>/clipboard` is attacker-writable
//! — the guest can rename directories, plant symlinks and swap files at
//! any moment. The host therefore never trusts a *path* below the state
//! dir: [`ClipboardRoot`] opens `clipboard/` and each per-launch dir
//! with `O_NOFOLLOW`, keeps the descriptors, and does every later
//! operation through `/proc/self/fd/<fd>/<name>`, which resolves to the
//! inode we opened no matter what the guest has renamed since. Nothing
//! the guest writes is ever read back by the host, only listed and
//! unlinked. Liveness of sibling dirs is decided by a `flock`ed lock
//! file, not by PID existence (which lies across PID namespaces).
//!
//! Set `AGENT_VM_NO_CLIPBOARD_BRIDGE=1` to skip the wrapper entirely
//! (the agent then runs exactly as before, straight on the terminal).

use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::clipboard::which;
use crate::host_paths::atomic_write;
use crate::session::ProjectSession;

/// Env var the wrapper sets for its child: the **guest** path of this
/// launch's clipboard directory (`/agent-vm-state/clipboard/<pid>`).
/// Its presence is also the "I am the child, don't wrap again" marker.
/// `run.rs` turns it into a PATH prefix via [`guest_shim_bin_from_env`];
/// the child never touches the host side of the directory.
pub const CHILD_ENV: &str = "AGENT_VM_CLIPBOARD_DIR";

/// Opt-out: skip the pty wrapper and the in-guest shims.
pub const DISABLE_ENV: &str = "AGENT_VM_NO_CLIPBOARD_BRIDGE";

/// Where `<state>/clipboard` lands inside the guest: the whole state
/// dir is bind-mounted at [`crate::run::GUEST_STATE_MOUNT`] (a test
/// pins the two together).
pub const GUEST_ROOT: &str = "/agent-vm-state/clipboard";

/// Name of the subdirectory under the host state dir holding the
/// per-launch dirs.
const HOST_SUBDIR: &str = "clipboard";

/// Per-tool budget for reading the host clipboard. `wl-paste` blocks
/// forever without a compositor and `xclip` when the selection owner
/// is unresponsive. The relay is paused meanwhile (it must be: the key
/// is forwarded only after the snapshot exists), so keep this short.
const CLIPBOARD_TOOL_TIMEOUT: Duration = Duration::from_secs(3);

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
///
/// Errors are only possible before the child is spawned (the caller
/// can then safely launch unwrapped); once the child runs, every
/// failure is reported on stderr and turned into the child's exit.
pub fn maybe_wrap(mode: PasteMode) -> Result<Option<i32>> {
    if std::env::var_os(CHILD_ENV).is_some() {
        return Ok(None);
    }
    let session = match ProjectSession::for_cwd() {
        Ok(s) => s,
        // Let the real launch produce the (same) error message.
        Err(_) => return Ok(None),
    };
    session.ensure_dirs()?;
    let root = ClipboardRoot::open(&session.state_dir)
        .context("opening the clipboard bridge directory")?;
    // Sweep leftovers of crashed wrappers *before* deciding whether to
    // wrap: a launch with the bridge disabled (or without a terminal)
    // must not leave a previous session's host screenshots visible to
    // its guest.
    root.sweep_stale();

    if std::env::var_os(DISABLE_ENV).is_some() {
        return Ok(None);
    }
    if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
        // No interactive terminal: attach() won't be used either, and
        // there is nobody to press Ctrl+V.
        return Ok(None);
    }

    let dir = root
        .create_session_dir(&std::process::id().to_string())
        .context("creating the clipboard bridge session directory")?;
    let outcome = relay(mode, &dir);
    dir.remove();
    let outcome = outcome?;
    if let Some(sig) = outcome.signal {
        // The child died from a signal: die the same way so a calling
        // script sees WIFSIGNALED, not a made-up exit code. Terminal
        // modes are already restored and the directory is gone.
        // SAFETY: plain libc calls; resetting to the default disposition
        // and re-raising is the standard "propagate a fatal signal" idiom.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }
    Ok(Some(outcome.code))
}

/// Guest PATH prefix for this launch, derived from [`CHILD_ENV`] in the
/// child. `None` when unset (unwrapped launch) or malformed.
pub fn guest_shim_bin_from_env() -> Option<String> {
    let value = std::env::var(CHILD_ENV).ok()?;
    match guest_shim_bin(&value) {
        Some(bin) => Some(bin),
        None => {
            eprintln!(
                "==> warning: ignoring malformed {CHILD_ENV}={value:?}; Ctrl+V image bridge disabled in guest"
            );
            None
        }
    }
}

/// `/agent-vm-state/clipboard/<digits>` → `…/<digits>/bin`. Strict on
/// purpose: the value ends up in the guest environment and PATH.
fn guest_shim_bin(value: &str) -> Option<String> {
    let rest = value.strip_prefix(GUEST_ROOT)?.strip_prefix('/')?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("{value}/bin"))
}

// ---------------------------------------------------------------------
// Host-side storage, addressed by descriptor
// ---------------------------------------------------------------------

/// `/proc/self/fd/<fd>/<name>`: a path that resolves through the
/// directory we hold open, immune to renames and symlink swaps of any
/// parent component by the guest. `O_NOFOLLOW` (or an `lstat`) still
/// has to guard the final component where it matters.
fn fd_path(dir: &OwnedFd, name: &str) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}/{name}", dir.as_raw_fd()))
}

/// The directory itself, for `read_dir`.
fn fd_dir(dir: &OwnedFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
}

/// Open `path` as a directory without following a symlink in its
/// final component.
fn open_dir_nofollow(path: &Path) -> io::Result<OwnedFd> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    Ok(OwnedFd::from(f))
}

/// Create (if needed) and open the subdirectory `name` of `parent`,
/// refusing a symlink or anything else the guest may have planted
/// under that name.
fn open_child_dir(parent: &OwnedFd, name: &str) -> io::Result<OwnedFd> {
    let path = fd_path(parent, name);
    match fs::create_dir(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    open_dir_nofollow(&path)
}

/// Remove `parent/name` whatever it is: a symlink or file is unlinked
/// (never followed), a real directory is removed recursively —
/// `remove_dir_all` handles the final component with `lstat` and
/// walks the tree with `O_NOFOLLOW` internally.
fn remove_entry(parent: &OwnedFd, name: &str) -> io::Result<()> {
    let path = fd_path(parent, name);
    match fs::symlink_metadata(&path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(&path),
        Ok(_) => fs::remove_file(&path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn flock_nb(f: &File) -> bool {
    // SAFETY: flock on a descriptor we own; no memory is involved.
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// `<state>/clipboard`, held open.
struct ClipboardRoot {
    fd: OwnedFd,
}

impl ClipboardRoot {
    fn open(state_dir: &Path) -> Result<Self> {
        // The state dir itself is host-owned (its parent is not shared
        // with the guest), so following symlinks in it is fine — a user
        // may well point AGENT_VM_STATE_DIR at one.
        let state = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(state_dir)
            .with_context(|| format!("opening {}", state_dir.display()))?;
        let state = OwnedFd::from(state);
        let fd = match open_child_dir(&state, HOST_SUBDIR) {
            Ok(fd) => fd,
            Err(_) => {
                // A symlink or file planted by the guest under the name:
                // unlink it (never followed) and try once more.
                let _ = fs::remove_file(fd_path(&state, HOST_SUBDIR));
                open_child_dir(&state, HOST_SUBDIR)
                    .with_context(|| format!("creating {}/{HOST_SUBDIR}", state_dir.display()))?
            }
        };
        Ok(Self { fd })
    }

    /// Create `<root>/<name>` fresh — a leftover under that name (PID
    /// reuse after a crash, or something the guest planted) is removed
    /// first — take its lock, and write the guest shims into `bin/`.
    fn create_session_dir(&self, name: &str) -> Result<SessionDir> {
        let path = fd_path(&self.fd, name);
        if let Err(e) = fs::create_dir(&path) {
            if e.kind() != io::ErrorKind::AlreadyExists {
                return Err(e).with_context(|| format!("creating session dir {name}"));
            }
            remove_entry(&self.fd, name)
                .with_context(|| format!("removing stale session dir {name}"))?;
            fs::create_dir(&path).with_context(|| format!("creating session dir {name}"))?;
        }
        let fd = open_dir_nofollow(&path).with_context(|| format!("opening session dir {name}"))?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(fd_path(&fd, LOCK_FILE))
            .context("creating the session lock file")?;
        if !flock_nb(&lock) {
            bail!("session dir {name} is locked by another process");
        }
        let guest_dir = format!("{GUEST_ROOT}/{name}");
        let bin = open_child_dir(&fd, "bin").context("creating the guest shim dir")?;
        for (shim, body) in [
            ("xclip", GUEST_SHIM_XCLIP),
            ("wl-paste", GUEST_SHIM_WL_PASTE),
        ] {
            let script = body.replace("@DIR@", &guest_dir);
            atomic_write(&fd_path(&bin, shim), script.as_bytes(), 0o755)
                .with_context(|| format!("writing clipboard shim {shim}"))?;
        }
        Ok(SessionDir {
            root: self.fd.try_clone().context("dup clipboard root fd")?,
            fd,
            _lock: lock,
            name: name.to_string(),
            guest_dir,
        })
    }

    /// Remove sibling session dirs whose wrapper is gone (crashed or
    /// SIGKILLed before its own cleanup) — i.e. whose lock file is not
    /// held. Anything under `<root>` that isn't a real directory named
    /// like a PID was planted by a guest and is unlinked. Best-effort.
    fn sweep_stale(&self) {
        let Ok(entries) = fs::read_dir(fd_dir(&self.fd)) else {
            return;
        };
        let own = std::process::id().to_string();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == own || name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let alive = open_dir_nofollow(&fd_path(&self.fd, name))
                .ok()
                .and_then(|dir| {
                    OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                        .open(fd_path(&dir, LOCK_FILE))
                        .ok()
                })
                // Lock held by a live wrapper → we can't take it.
                .map(|lock| !flock_nb(&lock))
                .unwrap_or(false);
            if !alive {
                let _ = remove_entry(&self.fd, name);
            }
        }
    }
}

const LOCK_FILE: &str = "lock";
const SNAPSHOT_PREFIX: &str = "paste-";
const SNAPSHOT_SUFFIX: &str = ".png";

/// `<state>/clipboard/<pid>`, held open together with its lock.
struct SessionDir {
    root: OwnedFd,
    fd: OwnedFd,
    _lock: File,
    name: String,
    /// `/agent-vm-state/clipboard/<pid>` as the guest sees it.
    guest_dir: String,
}

impl SessionDir {
    fn snapshot_name(n: u32) -> String {
        format!("{SNAPSHOT_PREFIX}{n:06}{SNAPSHOT_SUFFIX}")
    }

    /// Store `png` as snapshot number `n`; returns its guest path.
    fn write_snapshot(&self, n: u32, png: &[u8]) -> Result<String> {
        let name = Self::snapshot_name(n);
        atomic_write(&fd_path(&self.fd, &name), png, 0o600)
            .with_context(|| format!("writing snapshot {name}"))?;
        Ok(format!("{}/{name}", self.guest_dir))
    }

    fn remove_snapshots(&self) {
        let Ok(entries) = fs::read_dir(fd_dir(&self.fd)) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(SNAPSHOT_PREFIX) && name.ends_with(SNAPSHOT_SUFFIX) {
                // unlink never follows a symlink the guest may have
                // swapped in.
                let _ = fs::remove_file(fd_path(&self.fd, &name));
            }
        }
    }

    /// Delete the whole session dir. Consumes the handle so the lock
    /// is released only after the tree is gone.
    fn remove(self) {
        let _ = remove_entry(&self.root, &self.name);
    }
}

// ---------------------------------------------------------------------
// The pty relay
// ---------------------------------------------------------------------

/// Signals seen since the last poll iteration, one bit per number.
/// Handlers only ever `fetch_or` here — async-signal-safe.
static PENDING_SIGNALS: AtomicU64 = AtomicU64::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    PENDING_SIGNALS.fetch_or(1u64 << sig, Ordering::SeqCst);
}

const fn bit(sig: libc::c_int) -> u64 {
    1u64 << sig
}

/// Signals an external party sends *us* that must reach the child (as
/// its process group, so helpers it spawned see them too).
const FORWARDED: [libc::c_int; 4] = [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT];

fn install_signal_handlers() {
    for sig in [libc::SIGWINCH, libc::SIGCHLD, libc::SIGCONT]
        .into_iter()
        .chain(FORWARDED)
    {
        // SAFETY: `on_signal` is async-signal-safe (one atomic op). The
        // function-pointer cast is the documented way to hand libc a
        // handler.
        unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
    }
}

struct Outcome {
    code: i32,
    /// Set when the child died from a signal (`code` is then 128+n).
    signal: Option<libc::c_int>,
}

/// Bytes of relay output buffered for the child's pty beyond which we
/// stop reading the keyboard, so a child that isn't reading (busy
/// writing to a full pty we haven't drained) can't wedge us into a
/// write/write deadlock.
const MAX_BACKLOG: usize = 64 * 1024;

/// Spawn ourselves (same argv, plus [`CHILD_ENV`]) on a fresh pty and
/// pump bytes until the child exits. Terminal modes are restored on
/// every return path. Only failures *before* the child exists come
/// back as `Err`; afterwards the relay always ends in the child's
/// exit status.
fn relay(mode: PasteMode, dir: &SessionDir) -> Result<Outcome> {
    let (master, slave) =
        openpty().context("allocating a pseudo-terminal for the clipboard bridge")?;

    // The child's pty starts out as an exact copy of the real terminal
    // (modes + window size), so the launcher's pre-attach phase and the
    // SDK's raw-mode dance behave exactly as without the wrapper.
    let orig = tcgetattr(libc::STDIN_FILENO).context("tcgetattr(stdin)")?;
    tcsetattr(slave.as_raw_fd(), &orig).context("tcsetattr(pty)")?;

    // Raw mode on the real terminal: every key (Ctrl+C, Ctrl+Z, ...)
    // travels to the child's pty as bytes, whose own line discipline
    // decides what they mean — exactly as if the child sat on the
    // terminal itself. Done before the spawn so that nothing after it
    // can fail and leave a running child behind.
    let mut raw = orig;
    // SAFETY: cfmakeraw only writes the termios struct it is given.
    unsafe { libc::cfmakeraw(&mut raw) };
    tcsetattr(libc::STDIN_FILENO, &raw).context("entering raw mode")?;
    let _restore = scopeguard(move || {
        let _ = tcsetattr(libc::STDIN_FILENO, &orig);
    });
    // Handlers go in before the spawn (execve resets them in the child)
    // so a resize between the initial size copy and the loop isn't lost.
    install_signal_handlers();
    copy_winsize(libc::STDIN_FILENO, slave.as_raw_fd());
    set_nonblocking(master.as_raw_fd());

    let exe = std::env::current_exe().context("std::env::current_exe")?;
    let mut cmd = Command::new(exe);
    cmd.args(std::env::args_os().skip(1));
    cmd.env(CHILD_ENV, &dir.guest_dir);
    cmd.stdin(Stdio::from(slave.try_clone().context("dup pty")?));
    cmd.stdout(Stdio::from(slave.try_clone().context("dup pty")?));
    // `agent-vm claude 2>log` must keep working: only route stderr
    // through the pty when it *is* the terminal.
    if io::stderr().is_terminal() {
        cmd.stderr(Stdio::from(slave));
    } else {
        drop(slave);
        cmd.stderr(Stdio::inherit());
    }
    // Make the pty the child's controlling terminal: SIGWINCH from our
    // TIOCSWINSZ below, SIGHUP when we go away, job control — all the
    // semantics the launcher had when it sat on the real terminal.
    // SAFETY: the closure runs between fork and exec and calls only
    // async-signal-safe functions (setsid, ioctl).
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
    let child = cmd
        .spawn()
        .context("re-executing agent-vm under the clipboard bridge")?;
    // Drop our copies of the slave end now, otherwise the master never
    // reports EIO when the child exits and we'd hang forever.
    drop(cmd);
    let child_pid = child.id() as libc::pid_t;
    // From here on the child is reaped by hand (waitpid with WUNTRACED
    // for job control), never through `Child::wait`.
    std::mem::forget(child);

    let mut bridge = Bridge {
        dir,
        mode,
        counter: 0,
        paste_enabled: false,
    };
    let mut scanner = KeyScanner::default();
    let mut stdout = io::stdout().lock();
    let mut stdin_open = true;
    let mut backlog: Vec<u8> = Vec::new();
    let mut status: Option<libc::c_int> = None;
    let mut buf = [0u8; 4096];

    loop {
        let pending = PENDING_SIGNALS.swap(0, Ordering::SeqCst);
        if pending & bit(libc::SIGWINCH) != 0 {
            // The kernel raises SIGWINCH in the child for us on resize.
            copy_winsize(libc::STDIN_FILENO, master.as_raw_fd());
        }
        if pending & bit(libc::SIGCONT) != 0 {
            // We were resumed after stopping the job below: back to raw
            // mode, and wake the child too.
            let _ = tcsetattr(libc::STDIN_FILENO, &raw);
            copy_winsize(libc::STDIN_FILENO, master.as_raw_fd());
            kill_group(child_pid, libc::SIGCONT);
        }
        for sig in FORWARDED {
            if pending & bit(sig) != 0 {
                kill_group(child_pid, sig);
                // A stopped child wouldn't act on it otherwise.
                kill_group(child_pid, libc::SIGCONT);
            }
        }
        if pending & bit(libc::SIGCHLD) != 0 {
            match wait_child(
                child_pid,
                libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED,
            ) {
                Some(st) if libc::WIFSTOPPED(st) => {
                    // Ctrl+Z reached the child's pty while its line
                    // discipline still had ISIG on (before attach()).
                    // Behave like `script`/`ssh`: stop this job too, so
                    // the user's shell gets the terminal back and `fg`
                    // works. The SIGCONT branch above undoes it.
                    let _ = tcsetattr(libc::STDIN_FILENO, &orig);
                    // SAFETY: raising SIGTSTP on ourselves; default
                    // disposition (stop), nothing to clean up.
                    unsafe { libc::raise(libc::SIGTSTP) };
                }
                Some(st) if libc::WIFEXITED(st) || libc::WIFSIGNALED(st) => {
                    status = Some(st);
                    // Keep draining the master until EIO so no output
                    // is lost.
                }
                _ => {}
            }
        }
        let mut fds = [
            libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN | if backlog.is_empty() { 0 } else { libc::POLLOUT },
                revents: 0,
            },
            libc::pollfd {
                // -1 = ignored: a hung-up tty reports POLLHUP regardless
                // of `events`, which would spin the loop.
                fd: if stdin_open && backlog.len() < MAX_BACKLOG {
                    libc::STDIN_FILENO
                } else {
                    -1
                },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // A held-back partial escape sequence is flushed after a short
        // silence so a lone Esc key isn't delayed noticeably.
        let timeout_ms = if scanner.has_pending() { 25 } else { 100 };
        // SAFETY: `fds` is a valid array of the stated length.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout_ms) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            eprintln!("\r\nagent-vm: clipboard bridge: poll failed: {err}\r");
            break;
        }
        if n == 0 && scanner.has_pending() {
            backlog.extend(scanner.flush());
        }

        if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match read_fd(master.as_raw_fd(), &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    bridge.observe_output(&buf[..n]);
                    stdout.write_all(&buf[..n]).ok();
                    stdout.flush().ok();
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                // EIO: the child closed its side (exited). Normal.
                Err(_) => break,
            }
        }

        if fds[1].fd >= 0 && fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match read_fd(libc::STDIN_FILENO, &mut buf) {
                Ok(0) => stdin_open = false,
                Ok(n) => {
                    let out = scanner.scan(&buf[..n], &mut |out, key| bridge.on_ctrl_v(out, key));
                    backlog.extend(out);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => stdin_open = false,
            }
        }

        if !backlog.is_empty() {
            match drain_to(master.as_raw_fd(), &mut backlog) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("\r\nagent-vm: clipboard bridge: writing to the agent's pty: {e}\r");
                    break;
                }
            }
        }
    }

    // Closing the master hangs up the child's terminal; a no-op when it
    // already exited (the usual case), the equivalent of the user
    // closing the terminal window if we bailed out of the loop early.
    drop(master);
    let st = match status {
        Some(st) => st,
        None => loop {
            match wait_child(child_pid, 0) {
                Some(st) if libc::WIFEXITED(st) || libc::WIFSIGNALED(st) => break st,
                Some(_) => continue,
                None => break 0,
            }
        },
    };
    Ok(decode_status(st))
}

/// Map a raw `waitpid` status to what the shell would report.
fn decode_status(st: libc::c_int) -> Outcome {
    if libc::WIFSIGNALED(st) {
        let sig = libc::WTERMSIG(st);
        Outcome {
            code: 128 + sig,
            signal: Some(sig),
        }
    } else if libc::WIFEXITED(st) {
        Outcome {
            code: libc::WEXITSTATUS(st),
            signal: None,
        }
    } else {
        Outcome {
            code: 1,
            signal: None,
        }
    }
}

/// `waitpid(pid, flags)`; `Some(status)` when it reported something
/// about `pid`, `None` on WNOHANG-nothing-yet or error (ECHILD).
fn wait_child(pid: libc::pid_t, flags: libc::c_int) -> Option<libc::c_int> {
    let mut st: libc::c_int = 0;
    loop {
        // SAFETY: `st` is a valid out-pointer for the duration of the call.
        let r = unsafe { libc::waitpid(pid, &mut st, flags) };
        if r == pid {
            return Some(st);
        }
        if r == -1 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return None;
    }
}

/// Signal the child's whole process group (it is a session leader, so
/// its pgid equals its pid).
fn kill_group(pid: libc::pid_t, sig: libc::c_int) {
    // SAFETY: kill has no memory-safety preconditions.
    unsafe { libc::kill(-pid, sig) };
}

fn openpty() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: out-pointers to two valid ints; null for name/termios/winsize
    // is documented as "don't care".
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
        // SAFETY: fcntl on descriptors we just received.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    // SAFETY: openpty handed us two fresh, owned descriptors.
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

fn set_nonblocking(fd: RawFd) {
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags != -1 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

fn tcgetattr(fd: RawFd) -> io::Result<libc::termios> {
    let mut t = MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr fills the struct on success, which is the only
    // case in which we assume it initialised.
    if unsafe { libc::tcgetattr(fd, t.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { t.assume_init() })
}

fn tcsetattr(fd: RawFd, t: &libc::termios) -> io::Result<()> {
    // SAFETY: `t` is a fully initialised termios.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn copy_winsize(from: RawFd, to: RawFd) {
    let mut ws = MaybeUninit::<libc::winsize>::uninit();
    // SAFETY: TIOCGWINSZ fills `ws` before TIOCSWINSZ reads it; the
    // second ioctl only runs when the first succeeded.
    unsafe {
        if libc::ioctl(from, libc::TIOCGWINSZ as _, ws.as_mut_ptr()) == 0 {
            libc::ioctl(to, libc::TIOCSWINSZ as _, ws.as_ptr());
        }
    }
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buf` is valid for writes of `buf.len()` bytes.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Write as much of `backlog` to the non-blocking `fd` as it accepts,
/// keeping the rest for the next round.
fn drain_to(fd: RawFd, backlog: &mut Vec<u8>) -> io::Result<()> {
    let mut written = 0;
    while written < backlog.len() {
        // SAFETY: the slice is valid for reads of its length.
        let n = unsafe {
            libc::write(
                fd,
                backlog[written..].as_ptr().cast(),
                backlog.len() - written,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => break,
                _ => return Err(err),
            }
        }
        written += n as usize;
    }
    backlog.drain(..written);
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
// Ctrl+V handling
// ---------------------------------------------------------------------

/// Per-launch state behind the Ctrl+V callback.
struct Bridge<'a> {
    dir: &'a SessionDir,
    mode: PasteMode,
    counter: u32,
    /// Whether the agent currently has bracketed paste (DEC mode 2004)
    /// switched on, tracked from its output. Injecting `CSI 200 ~`
    /// into an application that never asked for it would deliver
    /// garbage keystrokes.
    paste_enabled: bool,
}

const BRACKETED_PASTE_ON: &[u8] = b"\x1b[?2004h";
const BRACKETED_PASTE_OFF: &[u8] = b"\x1b[?2004l";

impl Bridge<'_> {
    /// Called for every chunk the child writes to its terminal.
    fn observe_output(&mut self, out: &[u8]) {
        let on = rfind(out, BRACKETED_PASTE_ON);
        let off = rfind(out, BRACKETED_PASTE_OFF);
        match (on, off) {
            (Some(a), Some(b)) => self.paste_enabled = a > b,
            (Some(_), None) => self.paste_enabled = true,
            (None, Some(_)) => self.paste_enabled = false,
            (None, None) => {}
        }
    }

    /// The scanner found Ctrl+V (`key` = its bytes). Decide what the
    /// child receives instead.
    fn on_ctrl_v(&mut self, out: &mut Vec<u8>, key: &[u8]) {
        let guest_path = self.snapshot(read_host_clipboard_png());
        match (self.mode, guest_path) {
            (PasteMode::PastePath, Some(path)) if self.paste_enabled => {
                out.extend_from_slice(&bracketed_paste(&path));
            }
            _ => out.extend_from_slice(key),
        }
    }

    /// Store `png` (if any) as the next snapshot; returns its guest path.
    ///
    /// In `ForwardKey` mode the previous snapshots are removed first
    /// (also when there is no image now): the guest shims serve "the
    /// newest file", and a stale one would make a Ctrl+V with text on
    /// the clipboard paste last week's screenshot. Claude Code copies
    /// the bytes into memory as soon as it reads them, so nothing is
    /// lost. In `PastePath` mode the files stay: Codex only reads the
    /// path when the message is submitted, possibly several pastes
    /// later.
    fn snapshot(&mut self, png: Option<Vec<u8>>) -> Option<String> {
        if self.mode == PasteMode::ForwardKey {
            self.dir.remove_snapshots();
        }
        let png = png?;
        self.counter += 1;
        match self.dir.write_snapshot(self.counter, &png) {
            Ok(path) => Some(path),
            Err(e) => {
                eprintln!("\r\nagent-vm: clipboard bridge: {e:#}\r");
                None
            }
        }
    }
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
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
// Keyboard stream scanning
// ---------------------------------------------------------------------

const ESC: u8 = 0x1b;
const CTRL_V: u8 = 0x16;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// Longest escape sequence we care about (`ESC [ 118:86:118 ; 5:1 u`
/// and friends fit comfortably).
const MAX_HOLD: usize = 24;

/// Stateful Ctrl+V detector over the raw keyboard byte stream.
///
/// Recognises the legacy `0x16` byte, the kitty keyboard protocol
/// encoding `CSI 118 [:alt] ; <mods> [:event] u` (Ctrl bit set, press
/// or repeat event) and xterm's `modifyOtherKeys` form
/// `CSI 27 ; <mods> ; 118 ~`. Bytes inside a bracketed paste
/// (`CSI 200 ~` .. `CSI 201 ~`) are never interpreted — pasted *text*
/// may legitimately contain a ^V. Everything not recognised is passed
/// through verbatim.
///
/// Terminals write a key's sequence in one go, but the pty can still
/// split it across our reads (a paste marker at a 4 KiB boundary of a
/// large paste, say). A trailing byte run that could still grow into
/// one of the sequences above is therefore held back and prepended to
/// the next chunk; the relay flushes it after a short silence.
#[derive(Default)]
struct KeyScanner {
    in_paste: bool,
    pending: Vec<u8>,
}

impl KeyScanner {
    /// Scan one chunk read from the terminal. Returns the bytes to
    /// forward now; `on_ctrl_v(out, key_bytes)` is invoked for each
    /// Ctrl+V and decides what to append in place of the key.
    fn scan(&mut self, chunk: &[u8], on_ctrl_v: &mut dyn FnMut(&mut Vec<u8>, &[u8])) -> Vec<u8> {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(chunk);
        let hold = held_suffix_len(&data);
        let (now, later) = data.split_at(data.len() - hold);
        let out = self.scan_complete(now, on_ctrl_v);
        self.pending = later.to_vec();
        out
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Give up on a held-back partial sequence: it wasn't one.
    fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    fn scan_complete(
        &mut self,
        data: &[u8],
        on_ctrl_v: &mut dyn FnMut(&mut Vec<u8>, &[u8]),
    ) -> Vec<u8> {
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
                match parse_key_report(&data[i..]) {
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

/// Length of the trailing run of `data` that is a proper prefix of a
/// sequence we recognise (`ESC`, `ESC [`, `ESC [ 2 0`, `ESC [ 118 ; 5`
/// ...): those bytes must wait for the next chunk before scanning.
fn held_suffix_len(data: &[u8]) -> usize {
    let window = &data[data.len().saturating_sub(MAX_HOLD)..];
    let Some(esc) = window.iter().rposition(|&b| b == ESC) else {
        return 0;
    };
    let rest = &window[esc + 1..];
    let partial = match rest.first() {
        None => true,
        Some(b'[') => rest[1..]
            .iter()
            .all(|b| b.is_ascii_digit() || *b == b':' || *b == b';'),
        Some(_) => false,
    };
    if partial { window.len() - esc } else { 0 }
}

/// Parse a CSI key report at the start of `s`:
///
/// * kitty: `ESC [ key[:alt[:base]] [; mods[:event]] u`
/// * xterm modifyOtherKeys: `ESC [ 27 ; mods ; key ~`
///
/// Returns `(sequence_len, is_ctrl_v_press)`, or `None` if `s` doesn't
/// start with a complete report of either shape.
fn parse_key_report(s: &[u8]) -> Option<(usize, bool)> {
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
    let first = num(&mut i)?;
    // Optional alternate key codes (shifted key, base-layout key).
    for _ in 0..2 {
        if s.get(i) == Some(&b':') {
            i += 1;
            num(&mut i)?;
        }
    }
    let mut mods = 1;
    let mut event = 1;
    let mut key = first;
    if s.get(i) == Some(&b';') {
        i += 1;
        mods = num(&mut i)?;
        if s.get(i) == Some(&b':') {
            i += 1;
            event = num(&mut i)?;
        } else if first == 27 && s.get(i) == Some(&b';') {
            // modifyOtherKeys: the key code comes last.
            i += 1;
            key = num(&mut i)?;
            if s.get(i) != Some(&b'~') {
                return None;
            }
            return Some((i + 1, key == 118 && has_ctrl(mods)));
        }
    }
    if s.get(i) != Some(&b'u') {
        return None;
    }
    let press = event == 1 || event == 2;
    Some((i + 1, key == 118 && has_ctrl(mods) && press))
}

/// Modifier field of a CSI key report: 1 + bitmask (shift 1, alt 2, ctrl 4, ...).
fn has_ctrl(mods: u32) -> bool {
    mods >= 1 && ((mods - 1) & 4) != 0
}

// ---------------------------------------------------------------------
// Host clipboard
// ---------------------------------------------------------------------

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Read the host clipboard as PNG, trying the tools of the running
/// session type first (Wayland vs X11). `None` when the clipboard
/// holds no image or no tool is available.
fn read_host_clipboard_png() -> Option<Vec<u8>> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    for (cmd, args) in clipboard_tool_order(wayland, x11) {
        if which(cmd).is_none() {
            continue;
        }
        if let Some(bytes) = run_with_timeout(cmd, args, CLIPBOARD_TOOL_TIMEOUT)
            && bytes.starts_with(PNG_MAGIC)
        {
            return Some(bytes);
        }
    }
    None
}

const WL_PASTE: (&str, &[&str]) = ("wl-paste", &["--no-newline", "--type", "image/png"]);
const XCLIP: (&str, &[&str]) = (
    "xclip",
    &["-selection", "clipboard", "-t", "image/png", "-o"],
);

fn clipboard_tool_order(wayland: bool, x11: bool) -> [(&'static str, &'static [&'static str]); 2] {
    if !wayland && x11 {
        [XCLIP, WL_PASTE]
    } else {
        [WL_PASTE, XCLIP]
    }
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
    // pipe and deadlock against our wait loop. The result comes back
    // over a channel rather than `join()`: if the killed tool had
    // forked a helper that still holds the pipe, the thread lingers
    // until that helper goes away, and we must not wait for it.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut v = Vec::new();
        out.read_to_end(&mut v).ok();
        let _ = tx.send(v);
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
    let bytes = rx.recv_timeout(Duration::from_millis(500)).ok()?;
    match status {
        Some(s) if s.success() && !bytes.is_empty() => Some(bytes),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// Guest shims (written by the wrapper, executed inside the VM)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

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
        assert!(!s.has_pending());
    }

    #[test]
    fn legacy_ctrl_v_is_detected_and_forwarded() {
        let mut s = KeyScanner::default();
        let (out, hits) = scan_all(&mut s, b"ab\x16cd");
        assert_eq!(out, b"ab\x16cd");
        assert_eq!(hits, vec![b"\x16".to_vec()]);
    }

    #[test]
    fn kitty_and_xterm_ctrl_v_forms_are_detected() {
        for seq in [
            &b"\x1b[118;5u"[..],
            b"\x1b[118;5:1u",
            b"\x1b[118;5:2u",
            b"\x1b[118:86;6u", // ctrl+shift, alternate (shifted) key code
            b"\x1b[118:86:118;5u",
            b"\x1b[118;7u",    // ctrl+alt
            b"\x1b[27;5;118~", // xterm modifyOtherKeys
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
    fn key_report_non_matches_are_ignored() {
        for seq in [
            &b"\x1b[118;5:3u"[..], // release event
            b"\x1b[118u",          // no modifier field
            b"\x1b[118;1u",        // no modifier
            b"\x1b[118;2u",        // shift only
            b"\x1b[99;5u",         // ctrl+c
            b"\x1b[27;2;118~",     // shift+v (xterm)
            b"\x1b[27;5;99~",      // ctrl+c (xterm)
            b"\x1b[118;5~",        // wrong terminator
            b"\x1b[?0u",           // kitty flags query reply
            b"\x1b[1234567;5u",    // overlong number
        ] {
            let mut s = KeyScanner::default();
            let (out, hits) = scan_all(&mut s, seq);
            let out = [out, s.flush()].concat();
            assert_eq!(out, seq, "{:?}", String::from_utf8_lossy(seq));
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
    fn paste_markers_split_across_chunks_are_still_recognised() {
        // End marker split: `ESC [ 20` | `1 ~`.
        let mut s = KeyScanner::default();
        let (out, _) = scan_all(&mut s, b"\x1b[200~text\x1b[20");
        assert_eq!(out, b"\x1b[200~text");
        assert!(s.has_pending());
        let (out, hits) = scan_all(&mut s, b"1~\x16");
        assert_eq!(out, b"\x1b[201~\x16");
        assert_eq!(
            hits.len(),
            1,
            "Ctrl+V after the reassembled end marker counts"
        );
        assert!(!s.in_paste);

        // Start marker split: `ESC` | `[200~ ^V`.
        let mut s = KeyScanner::default();
        let (out, hits) = scan_all(&mut s, b"x\x1b");
        assert_eq!(out, b"x");
        assert!(hits.is_empty());
        let (out, hits) = scan_all(&mut s, b"[200~\x16");
        assert_eq!(out, b"\x1b[200~\x16");
        assert!(hits.is_empty(), "^V inside the reassembled paste is data");
        assert!(s.in_paste);
    }

    #[test]
    fn kitty_report_split_across_chunks_is_detected() {
        let mut s = KeyScanner::default();
        let (out, hits) = scan_all(&mut s, b"\x1b[118;");
        assert!(out.is_empty() && hits.is_empty());
        let (out, hits) = scan_all(&mut s, b"5u");
        assert_eq!(out, b"\x1b[118;5u");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn lone_escape_is_held_then_flushed_verbatim() {
        let mut s = KeyScanner::default();
        let (out, _) = scan_all(&mut s, b"\x1b");
        assert!(out.is_empty());
        assert!(s.has_pending());
        assert_eq!(s.flush(), b"\x1b");
        assert!(!s.has_pending());
        // Complete sequences are never held: alt+a, arrow key.
        let (out, _) = scan_all(&mut s, b"\x1ba\x1b[A");
        assert_eq!(out, b"\x1ba\x1b[A");
        assert!(!s.has_pending());
    }

    #[test]
    fn held_suffix_is_bounded() {
        // An ESC followed by a long run of digits stops looking like a
        // key report once it exceeds MAX_HOLD and is forwarded.
        let mut data = b"\x1b[".to_vec();
        data.extend(std::iter::repeat_n(b'1', MAX_HOLD));
        assert_eq!(held_suffix_len(&data), 0);
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
        assert_eq!(SessionDir::snapshot_name(1), "paste-000001.png");
        assert!(SessionDir::snapshot_name(9) < SessionDir::snapshot_name(10));
        assert!(SessionDir::snapshot_name(99999) < SessionDir::snapshot_name(100000));
    }

    #[test]
    fn guest_shim_bin_accepts_only_the_expected_shape() {
        assert_eq!(
            guest_shim_bin("/agent-vm-state/clipboard/4242").as_deref(),
            Some("/agent-vm-state/clipboard/4242/bin")
        );
        for bad in [
            "",
            "/agent-vm-state/clipboard",
            "/agent-vm-state/clipboard/",
            "/agent-vm-state/clipboard/abc",
            "/agent-vm-state/clipboard/42/../x",
            "/agent-vm-state/clipboard/42'",
            "/tmp/clipboard/42",
        ] {
            assert_eq!(guest_shim_bin(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn guest_root_lives_under_the_state_mount() {
        assert_eq!(
            GUEST_ROOT,
            format!("{}/{HOST_SUBDIR}", crate::run::GUEST_STATE_MOUNT)
        );
    }

    #[test]
    fn decode_status_maps_exit_and_signal() {
        let exited = decode_status(3 << 8);
        assert_eq!((exited.code, exited.signal), (3, None));
        let killed = decode_status(libc::SIGTERM);
        assert_eq!(
            (killed.code, killed.signal),
            (128 + libc::SIGTERM, Some(libc::SIGTERM))
        );
    }

    #[test]
    fn clipboard_tool_order_prefers_the_running_session_type() {
        assert_eq!(clipboard_tool_order(true, true)[0].0, "wl-paste");
        assert_eq!(clipboard_tool_order(true, false)[0].0, "wl-paste");
        assert_eq!(clipboard_tool_order(false, true)[0].0, "xclip");
        assert_eq!(clipboard_tool_order(false, false)[0].0, "wl-paste");
    }

    #[test]
    fn bracketed_paste_mode_is_tracked_from_output() {
        let tmp = tempfile::tempdir().unwrap();
        let root = ClipboardRoot::open(tmp.path()).unwrap();
        let dir = root.create_session_dir("1").unwrap();
        let mut b = Bridge {
            dir: &dir,
            mode: PasteMode::PastePath,
            counter: 0,
            paste_enabled: false,
        };
        b.observe_output(b"hello");
        assert!(!b.paste_enabled);
        b.observe_output(b"\x1b[?2004h\x1b[?25l");
        assert!(b.paste_enabled);
        b.observe_output(b"\x1b[?2004h...\x1b[?2004l");
        assert!(!b.paste_enabled);
        b.observe_output(b"\x1b[?2004l...\x1b[?2004h");
        assert!(b.paste_enabled);
    }

    #[test]
    fn run_with_timeout_returns_stdout_or_kills_a_hung_tool() {
        let _guard = crate::test_env::guard();
        assert_eq!(
            run_with_timeout("sh", &["-c", "printf abc"], Duration::from_secs(5)),
            Some(b"abc".to_vec())
        );
        // Non-zero exit and empty output are both "no image".
        assert_eq!(
            run_with_timeout("sh", &["-c", "printf abc; exit 1"], Duration::from_secs(5)),
            None
        );
        assert_eq!(
            run_with_timeout("sh", &["-c", "exit 0"], Duration::from_secs(5)),
            None
        );
        let t = Instant::now();
        assert_eq!(
            run_with_timeout("sh", &["-c", "sleep 5"], Duration::from_millis(100)),
            None
        );
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "hung tool must be killed at the deadline"
        );
        assert_eq!(
            run_with_timeout("/nonexistent/tool", &[], Duration::from_secs(1)),
            None
        );
    }

    fn list(dir: &SessionDir) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(fd_dir(&dir.fd))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn forward_key_snapshots_replace_each_other_and_clear_without_image() {
        let tmp = tempfile::tempdir().unwrap();
        let root = ClipboardRoot::open(tmp.path()).unwrap();
        let dir = root.create_session_dir("7").unwrap();
        let mut b = Bridge {
            dir: &dir,
            mode: PasteMode::ForwardKey,
            counter: 0,
            paste_enabled: false,
        };
        assert_eq!(
            b.snapshot(Some(b"A".to_vec())).as_deref(),
            Some("/agent-vm-state/clipboard/7/paste-000001.png")
        );
        assert_eq!(list(&dir), ["bin", "lock", "paste-000001.png"]);
        let mode = fs::metadata(tmp.path().join("clipboard/7/paste-000001.png"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        assert_eq!(
            b.snapshot(Some(b"B".to_vec())).as_deref(),
            Some("/agent-vm-state/clipboard/7/paste-000002.png")
        );
        assert_eq!(list(&dir), ["bin", "lock", "paste-000002.png"]);
        assert_eq!(
            fs::read(tmp.path().join("clipboard/7/paste-000002.png")).unwrap(),
            b"B"
        );

        // Text on the host clipboard: nothing new, and the old one goes
        // so the guest shim can't serve a stale screenshot.
        assert_eq!(b.snapshot(None), None);
        assert_eq!(list(&dir), ["bin", "lock"]);

        dir.remove();
        assert!(!tmp.path().join("clipboard/7").exists());
    }

    #[test]
    fn paste_path_snapshots_accumulate_until_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = ClipboardRoot::open(tmp.path()).unwrap();
        let dir = root.create_session_dir("8").unwrap();
        let mut b = Bridge {
            dir: &dir,
            mode: PasteMode::PastePath,
            counter: 0,
            paste_enabled: true,
        };
        b.snapshot(Some(b"A".to_vec()));
        b.snapshot(None);
        b.snapshot(Some(b"C".to_vec()));
        assert_eq!(
            list(&dir),
            ["bin", "lock", "paste-000001.png", "paste-000002.png"]
        );

        // The callback injects the path only while bracketed paste is on.
        let mut out = Vec::new();
        b.paste_enabled = false;
        b.on_ctrl_v(&mut out, b"\x16");
        assert!(out.starts_with(b"\x16") || out == b"\x16", "{out:?}");
        dir.remove();
    }

    #[test]
    fn sweep_removes_only_unlocked_pid_dirs_and_planted_junk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = ClipboardRoot::open(tmp.path()).unwrap();
        let clip = tmp.path().join("clipboard");
        // A live sibling holds its lock.
        let live = root.create_session_dir("100").unwrap();
        // A crashed sibling: dir with an unlocked lock file and a snapshot.
        fs::create_dir(clip.join("200")).unwrap();
        fs::write(clip.join("200/lock"), b"").unwrap();
        fs::write(clip.join("200/paste-000001.png"), b"secret").unwrap();
        // A dir without any lock file (older layout / guest-made).
        fs::create_dir(clip.join("300")).unwrap();
        // Guest-planted symlink named like a PID, pointing at a host dir
        // that must survive.
        let victim = tmp.path().join("victim");
        fs::create_dir(&victim).unwrap();
        fs::write(victim.join("keep"), b"").unwrap();
        symlink(&victim, clip.join("400")).unwrap();
        // Non-PID names are left alone.
        fs::create_dir(clip.join("notapid")).unwrap();

        root.sweep_stale();

        assert!(clip.join("100").is_dir(), "live sibling kept");
        assert!(!clip.join("200").exists(), "crashed sibling removed");
        assert!(!clip.join("300").exists(), "lockless dir removed");
        assert!(!clip.join("400").exists(), "planted symlink unlinked");
        assert!(victim.join("keep").exists(), "symlink target untouched");
        assert!(clip.join("notapid").is_dir());
        drop(live);
    }

    #[test]
    fn root_and_session_dir_refuse_planted_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        fs::create_dir(&state).unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();

        // `clipboard` itself is a symlink out of the state dir: it is
        // replaced by a real directory, the target untouched.
        symlink(&elsewhere, state.join("clipboard")).unwrap();
        let root = ClipboardRoot::open(&state).unwrap();
        assert!(state.join("clipboard").symlink_metadata().unwrap().is_dir());
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());

        // `<pid>` pre-planted as a symlink: unlinked, real dir created.
        symlink(&elsewhere, state.join("clipboard/55")).unwrap();
        let dir = root.create_session_dir("55").unwrap();
        assert!(
            state
                .join("clipboard/55")
                .symlink_metadata()
                .unwrap()
                .is_dir()
        );
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
        assert!(state.join("clipboard/55/bin/xclip").is_file());

        // The guest swaps our dir for a symlink mid-session: the held
        // descriptor still addresses the real (renamed) directory.
        fs::rename(state.join("clipboard/55"), state.join("clipboard/moved")).unwrap();
        symlink(&elsewhere, state.join("clipboard/55")).unwrap();
        dir.write_snapshot(1, b"png").unwrap();
        assert!(state.join("clipboard/moved/paste-000001.png").is_file());
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
        dir.remove_snapshots();
        assert!(!state.join("clipboard/moved/paste-000001.png").exists());
        // Exit cleanup removes whatever sits under our name (the
        // planted link) without following it.
        dir.remove();
        assert!(!state.join("clipboard/55").exists());
        assert!(elsewhere.is_dir());
    }

    #[test]
    fn guest_shims_serve_newest_snapshot_and_reject_text() {
        // Spawns /bin/sh: hold the crate-wide env lock (see test_env.rs).
        let _guard = crate::test_env::guard();
        let tmp = tempfile::tempdir().unwrap();
        let root = ClipboardRoot::open(tmp.path()).unwrap();
        let dir = root.create_session_dir("4242").unwrap();
        let host_dir = tmp.path().join("clipboard/4242");
        let bin = host_dir.join("bin");
        // The scripts embed the *guest* path; rewrite it to the host
        // temp dir so we can run them here.
        for name in ["xclip", "wl-paste"] {
            let p = bin.join(name);
            let body = fs::read_to_string(&p)
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

        fs::write(host_dir.join("paste-000001.png"), b"OLD").unwrap();
        fs::write(host_dir.join("paste-000002.png"), b"MID").unwrap();
        fs::write(host_dir.join("paste-000010.png"), b"NEW").unwrap();
        // An in-flight atomic_write must never be served.
        fs::write(host_dir.join("paste-000011.agent-vm-tmp"), b"PARTIAL").unwrap();

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
        dir.remove();
    }
}
