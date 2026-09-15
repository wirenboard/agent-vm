//! Ctrl+V image bridge for claude, opencode and codex.
//!
//! [`Bridge`] is a `StdinFilter` on the SDK's interactive attach. On
//! Ctrl+V it reads the host clipboard as PNG, writes it over agentd into
//! the guest tmpfs `/run/agent-vm/clipboard/paste-N.png`, then lets the
//! key through. Claude Code and OpenCode shell out to `xclip`/`wl-paste`,
//! so [`install`] puts a shim by each name first on the guest PATH that
//! serves the newest PNG. Codex reads the clipboard through X11/Wayland
//! (no shim possible) but attaches an image whose path is pasted, so its
//! Ctrl+V becomes a bracketed paste of the guest path.
//! Rationale: ARCHITECTURE.md, "Ctrl+V image paste".

use std::future::Future;
use std::io::{IsTerminal, Read};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use microsandbox::protocol::fs::FsSetAttrs;
use microsandbox::sandbox::{Sandbox, StdinFilter};

use crate::clipboard::which;

pub const DISABLE_ENV: &str = "AGENT_VM_NO_CLIPBOARD_BRIDGE";
pub const GUEST_DIR: &str = "/run/agent-vm/clipboard";
pub const GUEST_BIN: &str = "/run/agent-vm/clipboard/bin";
const TOOL_TIMEOUT: Duration = Duration::from_secs(3);
const GUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Snapshots kept in `PastePath` mode (Codex reads the file on submit).
const KEEP: u32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteMode {
    /// Forward the key; the guest shims serve the PNG (Claude Code, OpenCode).
    ForwardKey,
    /// Replace the key with a bracketed paste of the guest path (Codex).
    PastePath,
}

/// Opted in and driven by a terminal.
pub fn enabled() -> bool {
    std::env::var_os(DISABLE_ENV).is_none() && std::io::stdin().is_terminal()
}

/// Write the guest shims. Call before attaching.
pub async fn install(sandbox: &Sandbox) -> Result<()> {
    let fs = sandbox.fs();
    let write = async {
        fs.mkdir(GUEST_BIN).await.context("mkdir shim dir")?;
        for name in ["xclip", "wl-paste"] {
            let path = format!("{GUEST_BIN}/{name}");
            fs.write(&path, SHIM.replace("@DIR@", GUEST_DIR))
                .await
                .with_context(|| format!("writing {path}"))?;
            let mode = FsSetAttrs {
                mode: Some(0o755),
                ..Default::default()
            };
            fs.set_stat(&path, false, mode)
                .await
                .with_context(|| format!("chmod {path}"))?;
        }
        Ok(())
    };
    tokio::time::timeout(GUEST_TIMEOUT, write)
        .await
        .context("timed out")?
}

/// Guest-supplied error text, minus anything a terminal would interpret.
pub fn sanitize(err: impl std::fmt::Display) -> String {
    format!("{err:#}")
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

pub struct Bridge {
    sandbox: Sandbox,
    mode: PasteMode,
    scanner: KeyScanner,
    counter: u32,
}

impl Bridge {
    pub fn new(sandbox: Sandbox, mode: PasteMode) -> Self {
        Self {
            sandbox,
            mode,
            scanner: KeyScanner::default(),
            counter: 0,
        }
    }

    /// Push the host clipboard image into the guest; returns its guest path.
    async fn snapshot(&mut self) -> Option<String> {
        let png = tokio::task::spawn_blocking(read_host_clipboard_png)
            .await
            .ok()
            .flatten();
        let fs = self.sandbox.fs();
        // Drop the previous image first so a text-only clipboard can't
        // re-paste it.
        if self.mode == PasteMode::ForwardKey && self.counter > 0 {
            let _ = fs.remove(&snapshot_path(self.counter)).await;
        }
        let png = png?;
        self.counter += 1;
        let path = snapshot_path(self.counter);
        // Written under a name the shims' `paste-*.png` glob never matches
        // and the guest can't predict (agentd follows symlinks), then renamed.
        let nanos = SystemTime::UNIX_EPOCH
            .elapsed()
            .map_or(0, |d| d.subsec_nanos());
        let part = format!("{path}.{nanos:08x}.part");
        let write = async {
            fs.write(&part, png).await?;
            fs.rename(&part, &path).await
        };
        match tokio::time::timeout(GUEST_TIMEOUT, write).await {
            Ok(Ok(())) => {
                if self.mode == PasteMode::PastePath && self.counter > KEEP {
                    let _ = fs.remove(&snapshot_path(self.counter - KEEP)).await;
                }
                Some(path)
            }
            res => {
                let err = res.map_or("timed out".into(), |r| {
                    r.map_or_else(sanitize, |_| String::new())
                });
                eprintln!("\r\nagent-vm: clipboard bridge: writing {path}: {err}\r");
                let _ = fs.remove(&part).await;
                None
            }
        }
    }
}

fn snapshot_path(n: u32) -> String {
    format!("{GUEST_DIR}/paste-{n:06}.png")
}

impl StdinFilter for Bridge {
    fn filter<'a>(
        &'a mut self,
        data: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Vec<u8>> + Send + 'a>> {
        Box::pin(async move {
            let mut out = Vec::with_capacity(data.len());
            for seg in self.scanner.scan(data) {
                match seg {
                    Segment::Bytes(b) => out.extend_from_slice(&b),
                    Segment::CtrlV(key) => match (self.mode, self.snapshot().await) {
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
/// legacy `0x16`, kitty `CSI 118;<mods>u` (press only), xterm
/// `CSI 27;<mods>;118~`. Bytes inside a bracketed paste are never
/// interpreted; a trailing partial paste marker (`ESC[2…`) is held for
/// the next chunk so a marker split across reads is still seen.
#[derive(Default)]
struct KeyScanner {
    in_paste: bool,
    pending: Vec<u8>,
}

impl KeyScanner {
    fn scan(&mut self, chunk: &[u8]) -> Vec<Segment> {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(chunk);
        let hold = partial_marker(&data);
        self.pending = data.split_off(data.len() - hold);
        let mut segs = Vec::new();
        let mut bytes = Vec::new();
        let mut i = 0;
        while i < data.len() {
            if self.in_paste {
                match find(&data[i..], PASTE_END) {
                    Some(j) => {
                        let end = i + j + PASTE_END.len();
                        bytes.extend_from_slice(&data[i..end]);
                        self.in_paste = false;
                        i = end;
                    }
                    None => {
                        bytes.extend_from_slice(&data[i..]);
                        i = data.len();
                    }
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

/// Length of a trailing proper prefix of a paste marker, if at least
/// `ESC[2` (shorter tails are ordinary keys and must not be delayed).
fn partial_marker(data: &[u8]) -> usize {
    (3..PASTE_START.len())
        .rev()
        .find(|&n| data.ends_with(&PASTE_START[..n]) || data.ends_with(&PASTE_END[..n]))
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
            return (s.get(i) == Some(&b'~')).then_some((i + 1, key == 118 && ctrl_only(mods)));
        }
    }
    (s.get(i) == Some(&b'u')).then_some((i + 1, first == 118 && ctrl_only(mods) && event == 1))
}

/// Modifier field: 1 + bitmask (shift 1, alt 2, ctrl 4). Ctrl+Shift+V is
/// the terminal's own paste shortcut, so shift must be clear.
fn ctrl_only(mods: u32) -> bool {
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
    let x11_only =
        std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_some();
    let order = if x11_only {
        [XCLIP, WL_PASTE]
    } else {
        [WL_PASTE, XCLIP]
    };
    for (cmd, args) in order {
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

/// Run `cmd args`; stdout on success, `None` on failure or after `timeout`
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
    // Drained on a thread, collected via a channel: a killed tool's helper
    // still holding the pipe must not hang us.
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
// Guest shim
// ---------------------------------------------------------------------

/// Installed as both `xclip` and `wl-paste`; behaves per its own name.
/// Serves Claude Code's and OpenCode's probes: list types → `image/png`,
/// read `image/png` → newest snapshot; any other read (text, bmp) fails
/// as on a host without a clipboard tool.
const SHIM: &str = r#"#!/bin/sh
# agent-vm clipboard bridge: serves the host's Ctrl+V image snapshot.
DIR='@DIR@'
me=${0##*/}; list=false; png=false; out=false; want_type=false
for arg in "$@"; do
  if $want_type; then [ "$arg" = image/png ] && png=true; want_type=false; continue; fi
  case "$me:$arg" in
    xclip:TARGETS|wl-paste:-l|wl-paste:--list-types) list=true ;;
    xclip:image/png|wl-paste:--type=image/png) png=true ;;
    xclip:-o) out=true ;;
    wl-paste:-t|wl-paste:--type) want_type=true ;;
  esac
done
[ "$me" = wl-paste ] || $out || exit 1
latest=''
for f in "$DIR"/paste-*.png; do [ -f "$f" ] && latest="$f"; done
[ -n "$latest" ] || exit 1
if $list; then echo image/png; exit 0; fi
$png || exit 1
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
            b"\x1b[118;7u", // ctrl+alt
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
            b"\x1b[118u",
            b"\x1b[118;2u",
            b"\x1b[99;5u",
            b"\x1b[27;5;99~",
            b"\x1b[118;5~",
            b"\x1b[?0u",
            b"\x1b[1234567;5u",
            b"\x1b",
            b"\x1b[",
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
        // Split start marker: `ESC [ 2` | `00~ ^V`.
        let mut s = KeyScanner::default();
        assert_eq!(s.scan(b"x\x1b[2"), vec![bytes(b"x")]);
        assert_eq!(s.scan(b"00~\x16"), vec![bytes(b"\x1b[200~\x16")]);
        assert!(s.in_paste);
        // A false partial is released with the next chunk.
        let mut s = KeyScanner::default();
        assert_eq!(s.scan(b"\x1b[2"), vec![]);
        assert_eq!(s.scan(b"5~"), vec![bytes(b"\x1b[25~")]);
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
    fn shim_serves_newest_png_and_rejects_text() {
        let _guard = crate::test_env::guard();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_str().unwrap();
        for name in ["xclip", "wl-paste"] {
            crate::host_paths::atomic_write(
                &tmp.path().join(name),
                SHIM.replace("@DIR@", dir).as_bytes(),
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
        std::fs::write(
            tmp.path().join("paste-000011.png.0badf00d.part"),
            b"PARTIAL",
        )
        .unwrap();
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
        ok("wl-paste", &["--type=image/png"], b"NEW");
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
            ("xclip", &["-selection", "clipboard", "-t", "image/png"]), // no -o: a copy
            ("wl-paste", &["--type", "image/bmp"]),
            ("wl-paste", &["--no-newline"]),
            ("wl-paste", &[]),
        ] {
            assert_eq!(run(name, args).0, 1, "{name} {args:?}");
        }
    }
}
