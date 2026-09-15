//! Ctrl+V image bridge.
//!
//! The guest has no display server, so the agents' own clipboard access
//! fails there. During an interactive attach the SDK hands every chunk
//! of terminal input to [`Bridge`] (a `StdinFilter`). On Ctrl+V it reads
//! the host clipboard as PNG, pushes it over agentd into guest tmpfs
//! (`/run/agent-vm/clipboard/paste-N.png`) and then lets the key through:
//!
//! * Claude Code (and anything else that shells out) finds `xclip` /
//!   `wl-paste` shims first on the guest PATH that serve the newest PNG.
//! * Codex reads the clipboard via X11/Wayland directly, which can't be
//!   shimmed, but it attaches an image whose path is pasted — so its
//!   Ctrl+V is replaced by a bracketed paste of the guest path.
//!
//! Nothing touches the host disk or the guest-writable state mount, and
//! the files vanish with the VM. `AGENT_VM_NO_CLIPBOARD_BRIDGE=1` turns
//! the bridge off.

use std::future::Future;
use std::io::{IsTerminal, Read};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use microsandbox::protocol::fs::FsSetAttrs;
use microsandbox::sandbox::{Sandbox, StdinFilter};

use crate::clipboard::which;

pub const DISABLE_ENV: &str = "AGENT_VM_NO_CLIPBOARD_BRIDGE";
pub const GUEST_DIR: &str = "/run/agent-vm/clipboard";
pub const GUEST_BIN: &str = "/run/agent-vm/clipboard/bin";
const TOOL_TIMEOUT: Duration = Duration::from_secs(3);
/// Cap on one Ctrl+V (clipboard tools plus the guest write); past it the key
/// is forwarded as if there were no image.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);

/// What the guest receives for Ctrl+V when the host clipboard holds an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteMode {
    /// The key itself; the guest shims serve the PNG.
    ForwardKey,
    /// A bracketed paste of the PNG's guest path (Codex). Sent even before
    /// Codex enables bracketed paste; the filter can't see guest output.
    PastePath,
}

/// Bridge active for this launch: opted in and driven by a terminal.
pub fn enabled() -> bool {
    std::env::var_os(DISABLE_ENV).is_none() && std::io::stdin().is_terminal()
}

/// Write the guest shims. Call before attaching.
pub async fn install(sandbox: &Sandbox) -> Result<()> {
    let fs = sandbox.fs();
    fs.mkdir(GUEST_BIN).await.context("mkdir shim dir")?;
    for (name, body) in [("xclip", SHIM_XCLIP), ("wl-paste", SHIM_WL_PASTE)] {
        let path = format!("{GUEST_BIN}/{name}");
        fs.write(&path, body.replace("@DIR@", GUEST_DIR))
            .await
            .with_context(|| format!("writing {path}"))?;
        let attrs = FsSetAttrs {
            mode: Some(0o755),
            ..Default::default()
        };
        fs.set_stat(&path, false, attrs)
            .await
            .with_context(|| format!("chmod {path}"))?;
    }
    Ok(())
}

pub struct Bridge {
    sandbox: Sandbox,
    mode: PasteMode,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    scanner: KeyScanner,
    counter: u32,
    /// Previous snapshot, removed on the next Ctrl+V in `ForwardKey` mode
    /// so a text-only clipboard can't re-paste a stale image.
    previous: Option<String>,
}

impl Bridge {
    pub fn new(sandbox: Sandbox, mode: PasteMode) -> Self {
        Self {
            sandbox,
            mode,
            state: Mutex::new(State::default()),
        }
    }

    /// Push the host clipboard image into the guest; returns its guest path.
    async fn snapshot(&self) -> Option<String> {
        let png = tokio::task::spawn_blocking(read_host_clipboard_png)
            .await
            .ok()
            .flatten();
        let fs = self.sandbox.fs();
        let old = self.state.lock().unwrap().previous.take();
        if let (PasteMode::ForwardKey, Some(old)) = (self.mode, old) {
            let _ = fs.remove(&old).await;
        }
        let png = png?;
        let path = {
            let mut st = self.state.lock().unwrap();
            st.counter += 1;
            let path = format!("{GUEST_DIR}/paste-{:06}.png", st.counter);
            // Recorded before the write so a partial file is cleaned up next time.
            if self.mode == PasteMode::ForwardKey {
                st.previous = Some(path.clone());
            }
            path
        };
        match fs.write(&path, png).await {
            Ok(()) => Some(path),
            Err(e) => {
                let e: String = e.to_string().chars().filter(|c| !c.is_control()).collect();
                eprintln!("\r\nagent-vm: clipboard bridge: writing {path}: {e}\r");
                let _ = fs.remove(&path).await;
                None
            }
        }
    }
}

impl StdinFilter for Bridge {
    fn filter<'a>(&'a self, data: &'a [u8]) -> Pin<Box<dyn Future<Output = Vec<u8>> + Send + 'a>> {
        Box::pin(async move {
            let segments = self.state.lock().unwrap().scanner.scan(data);
            let mut out = Vec::with_capacity(data.len());
            for seg in segments {
                match seg {
                    Segment::Bytes(b) => out.extend_from_slice(&b),
                    Segment::CtrlV(key) => match (
                        self.mode,
                        tokio::time::timeout(SNAPSHOT_TIMEOUT, self.snapshot())
                            .await
                            .unwrap_or(None),
                    ) {
                        (PasteMode::PastePath, Some(path)) => {
                            out.extend_from_slice(PASTE_START);
                            out.extend_from_slice(path.as_bytes());
                            out.extend_from_slice(PASTE_END);
                        }
                        _ => out.extend_from_slice(&key),
                    },
                }
            }
            out
        })
    }
}

// ---------------------------------------------------------------------
// Keyboard stream scanning
// ---------------------------------------------------------------------

const ESC: u8 = 0x1b;
const CTRL_V: u8 = 0x16;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, PartialEq, Eq)]
enum Segment {
    Bytes(Vec<u8>),
    /// A Ctrl+V key report (its raw bytes).
    CtrlV(Vec<u8>),
}

/// Splits terminal input into passthrough bytes and Ctrl+V reports:
/// legacy `0x16`, kitty `CSI 118;<mods>u`, xterm `CSI 27;<mods>;118~`.
/// Bytes inside a bracketed paste are never interpreted. While inside a
/// paste, a trailing partial end marker is held for the next chunk so a
/// marker split across reads can't leave the guard stuck.
#[derive(Default)]
struct KeyScanner {
    in_paste: bool,
    pending: Vec<u8>,
}

impl KeyScanner {
    fn scan(&mut self, chunk: &[u8]) -> Vec<Segment> {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(chunk);
        let mut segs = Vec::new();
        let mut bytes = Vec::new();
        let mut i = 0;
        while i < data.len() {
            if self.in_paste {
                if let Some(j) = find(&data[i..], PASTE_END) {
                    let end = i + j + PASTE_END.len();
                    bytes.extend_from_slice(&data[i..end]);
                    self.in_paste = false;
                    i = end;
                } else {
                    let hold = partial_suffix(&data[i..], PASTE_END);
                    bytes.extend_from_slice(&data[i..data.len() - hold]);
                    self.pending = data[data.len() - hold..].to_vec();
                    break;
                }
                continue;
            }
            let b = data[i];
            if b == CTRL_V {
                push_bytes(&mut segs, &mut bytes);
                segs.push(Segment::CtrlV(vec![b]));
                i += 1;
            } else if b == ESC && data[i..].starts_with(PASTE_START) {
                bytes.extend_from_slice(PASTE_START);
                self.in_paste = true;
                i += PASTE_START.len();
            } else if let (true, Some((len, is_ctrl_v))) = (b == ESC, parse_key_report(&data[i..]))
            {
                if is_ctrl_v {
                    push_bytes(&mut segs, &mut bytes);
                    segs.push(Segment::CtrlV(data[i..i + len].to_vec()));
                } else {
                    bytes.extend_from_slice(&data[i..i + len]);
                }
                i += len;
            } else {
                bytes.push(b);
                i += 1;
            }
        }
        push_bytes(&mut segs, &mut bytes);
        segs
    }
}

fn push_bytes(segs: &mut Vec<Segment>, bytes: &mut Vec<u8>) {
    if !bytes.is_empty() {
        segs.push(Segment::Bytes(std::mem::take(bytes)));
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Length of the longest suffix of `data` that is a proper prefix of `seq`.
fn partial_suffix(data: &[u8], seq: &[u8]) -> usize {
    (1..seq.len())
        .rev()
        .find(|&n| data.ends_with(&seq[..n]))
        .unwrap_or(0)
}

/// Parse a CSI key report at the start of `s`: kitty
/// `ESC [ key[:alt[:base]] [; mods[:event]] u` or xterm modifyOtherKeys
/// `ESC [ 27 ; mods ; key ~`. Returns `(len, is_ctrl_v_press)`.
fn parse_key_report(s: &[u8]) -> Option<(usize, bool)> {
    let mut i = s.strip_prefix(b"\x1b[").map(|_| 2)?;
    let num = |i: &mut usize| -> Option<u32> {
        let start = *i;
        while *i < s.len() && s[*i].is_ascii_digit() {
            *i += 1;
        }
        (*i > start && *i - start <= 6)
            .then(|| std::str::from_utf8(&s[start..*i]).ok()?.parse().ok())?
    };
    let first = num(&mut i)?;
    for _ in 0..2 {
        if s.get(i) == Some(&b':') {
            i += 1;
            num(&mut i)?;
        }
    }
    let (mut mods, mut event) = (1, 1);
    if s.get(i) == Some(&b';') {
        i += 1;
        mods = num(&mut i)?;
        if s.get(i) == Some(&b':') {
            i += 1;
            event = num(&mut i)?;
        } else if first == 27 && s.get(i) == Some(&b';') {
            i += 1;
            let key = num(&mut i)?;
            return (s.get(i) == Some(&b'~')).then_some((i + 1, key == 118 && has_ctrl(mods)));
        }
    }
    (s.get(i) == Some(&b'u')).then_some((i + 1, first == 118 && has_ctrl(mods) && event == 1))
}

/// Modifier field of a key report: 1 + bitmask (shift 1, alt 2, ctrl 4).
/// Ctrl+Shift+V is the terminal's own paste shortcut, not ours.
fn has_ctrl(mods: u32) -> bool {
    mods >= 1 && (mods - 1) & 5 == 4
}

// ---------------------------------------------------------------------
// Host clipboard
// ---------------------------------------------------------------------

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
const WL_PASTE: (&str, &[&str]) = ("wl-paste", &["--no-newline", "--type", "image/png"]);
const XCLIP: (&str, &[&str]) = (
    "xclip",
    &["-selection", "clipboard", "-t", "image/png", "-o"],
);

fn read_host_clipboard_png() -> Option<Vec<u8>> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    for (cmd, args) in tool_order(wayland, x11) {
        if which(cmd).is_none() {
            continue;
        }
        if let Some(bytes) = run_with_timeout(cmd, args, TOOL_TIMEOUT)
            && bytes.starts_with(PNG_MAGIC)
        {
            return Some(bytes);
        }
    }
    None
}

fn tool_order(wayland: bool, x11: bool) -> [(&'static str, &'static [&'static str]); 2] {
    if !wayland && x11 {
        [XCLIP, WL_PASTE]
    } else {
        [WL_PASTE, XCLIP]
    }
}

/// Run `cmd args` and return its stdout on success; kill it at `timeout`
/// (`wl-paste` blocks forever without a compositor).
fn run_with_timeout(cmd: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Drain on a thread; a channel (not join) so a killed tool's lingering
    // helper holding the pipe can't hang us.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut v = Vec::new();
        stdout.read_to_end(&mut v).ok();
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
    (status?.success() && !bytes.is_empty()).then_some(bytes)
}

// ---------------------------------------------------------------------
// Guest shims
// ---------------------------------------------------------------------

/// Serves Claude Code's probes: `-t TARGETS -o` lists `image/png`,
/// `-t image/png -o` prints the newest snapshot; anything else (text,
/// bmp) fails like a host without a clipboard tool.
const SHIM_XCLIP: &str = r#"#!/bin/sh
# agent-vm clipboard bridge: serves the host's Ctrl+V image snapshot.
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

/// Wayland flavour: `-l` lists `image/png`, `--type image/png` prints
/// the newest snapshot, text reads fail.
const SHIM_WL_PASTE: &str = r#"#!/bin/sh
# agent-vm clipboard bridge: serves the host's Ctrl+V image snapshot.
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

    fn bytes(b: &[u8]) -> Segment {
        Segment::Bytes(b.to_vec())
    }
    fn ctrl_v(b: &[u8]) -> Segment {
        Segment::CtrlV(b.to_vec())
    }

    #[test]
    fn plain_and_legacy_ctrl_v() {
        let mut s = KeyScanner::default();
        assert_eq!(s.scan(b"hi\r\x1b[A\x03"), vec![bytes(b"hi\r\x1b[A\x03")]);
        assert_eq!(
            s.scan(b"ab\x16cd"),
            vec![bytes(b"ab"), ctrl_v(b"\x16"), bytes(b"cd")]
        );
    }

    #[test]
    fn kitty_and_xterm_reports() {
        for seq in [
            &b"\x1b[118;5u"[..],
            b"\x1b[118;5:1u",
            b"\x1b[118:86:118;5u",
            b"\x1b[118;7u",
            b"\x1b[27;5;118~",
        ] {
            assert_eq!(
                KeyScanner::default().scan(seq),
                vec![ctrl_v(seq)],
                "{seq:?}"
            );
        }
        for seq in [
            &b"\x1b[118;5:2u"[..], // repeat
            b"\x1b[118;5:3u",      // release
            b"\x1b[118:86;6u",     // ctrl+shift: the terminal's paste key
            b"\x1b[118u",          // no modifier field
            b"\x1b[118;2u",        // shift only
            b"\x1b[99;5u",         // ctrl+c
            b"\x1b[27;5;99~",
            b"\x1b[118;5~",
            b"\x1b[?0u",
            b"\x1b[1234567;5u",
            b"\x1b",
            b"\x1b[118;",
        ] {
            assert_eq!(KeyScanner::default().scan(seq), vec![bytes(seq)], "{seq:?}");
        }
    }

    #[test]
    fn bracketed_paste_guard_survives_chunk_splits() {
        let mut s = KeyScanner::default();
        assert_eq!(s.scan(b"\x1b[200~a\x16b"), vec![bytes(b"\x1b[200~a\x16b")]);
        // Split end marker: `ESC [ 20` | `1 ~`.
        assert_eq!(s.scan(b"\x16\x1b[20"), vec![bytes(b"\x16")]);
        assert_eq!(
            s.scan(b"1~\x16"),
            vec![bytes(b"\x1b[201~"), ctrl_v(b"\x16")]
        );
        assert!(!s.in_paste);
        // A false partial is released with the next chunk.
        let mut s = KeyScanner::default();
        assert_eq!(s.scan(b"\x1b[200~x\x1b"), vec![bytes(b"\x1b[200~x")]);
        assert_eq!(s.scan(b"y\x1b[201~"), vec![bytes(b"\x1by\x1b[201~")]);
    }

    #[test]
    fn tool_order_prefers_session_type() {
        assert_eq!(tool_order(true, true)[0].0, "wl-paste");
        assert_eq!(tool_order(false, true)[0].0, "xclip");
        assert_eq!(tool_order(false, false)[0].0, "wl-paste");
    }

    #[test]
    fn run_with_timeout_returns_stdout_or_kills() {
        let _guard = crate::test_env::guard();
        assert_eq!(
            run_with_timeout("sh", &["-c", "printf abc"], Duration::from_secs(5)),
            Some(b"abc".to_vec())
        );
        assert_eq!(
            run_with_timeout("sh", &["-c", "printf abc; exit 1"], Duration::from_secs(5)),
            None
        );
        let t = Instant::now();
        assert_eq!(
            run_with_timeout("sh", &["-c", "sleep 5"], Duration::from_millis(100)),
            None
        );
        assert!(t.elapsed() < Duration::from_secs(2));
        assert_eq!(
            run_with_timeout("/nonexistent", &[], Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn shims_serve_newest_png_and_reject_text() {
        let _guard = crate::test_env::guard();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_str().unwrap();
        for (name, body) in [("xclip", SHIM_XCLIP), ("wl-paste", SHIM_WL_PASTE)] {
            crate::host_paths::atomic_write(
                &tmp.path().join(name),
                body.replace("@DIR@", dir).as_bytes(),
                0o755,
            )
            .unwrap();
        }
        let run = |name: &str, args: &[&str]| {
            let out = Command::new(tmp.path().join(name))
                .args(args)
                .output()
                .unwrap();
            (out.status.code().unwrap(), out.stdout)
        };
        assert_eq!(
            run("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]).0,
            1
        );
        assert_eq!(run("wl-paste", &["-l"]).0, 1);

        std::fs::write(tmp.path().join("paste-000002.png"), b"OLD").unwrap();
        std::fs::write(tmp.path().join("paste-000010.png"), b"NEW").unwrap();
        let ok = |name, args: &[&str], want: &[u8]| {
            let (code, out) = run(name, args);
            assert_eq!((code, out.as_slice()), (0, want), "{name} {args:?}");
        };
        ok(
            "xclip",
            &["-selection", "clipboard", "-t", "TARGETS", "-o"],
            b"image/png\n",
        );
        ok(
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
            b"NEW",
        );
        ok("wl-paste", &["-l"], b"image/png\n");
        ok("wl-paste", &["--type", "image/png"], b"NEW");
        ok("wl-paste", &["-t", "image/png"], b"NEW");
        for (name, args) in [
            (
                "xclip",
                &["-selection", "clipboard", "-t", "image/bmp", "-o"][..],
            ),
            (
                "xclip",
                &["-selection", "clipboard", "-t", "text/plain", "-o"],
            ),
            ("xclip", &["-selection", "clipboard", "-o"]),
            ("wl-paste", &["--type", "image/bmp"]),
            ("wl-paste", &["--no-newline"]),
            ("wl-paste", &[]),
        ] {
            assert_eq!(run(name, args).0, 1, "{name} {args:?}");
        }
    }
}
