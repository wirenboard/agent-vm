#!/usr/bin/env python3
"""
PTY wrapper that intercepts Ctrl+V to save host clipboard images to a shared mount.

Sits between the host terminal and a child process (typically limactl shell).
When Ctrl+V (0x16) is detected in stdin, reads the clipboard image on the host
and writes it to $CLIPBOARD_DIR/clipboard.png. All bytes (including 0x16) are
always forwarded to the child.

Usage:
    CLIPBOARD_DIR=/path/to/shared python3 clipboard-pty.py limactl shell ...

Supports macOS (osascript/pbpaste) and Linux (wl-paste / xclip).
"""

import os
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import fcntl
import termios


CTRL_V = 0x16


def _read_clipboard_image():
    """Try to read PNG image data from the host clipboard. Returns bytes or None."""
    if sys.platform == "darwin":
        # macOS: use osascript to write clipboard PNGf data to a temp file
        try:
            result = subprocess.run(
                [
                    "osascript", "-e",
                    'set png to the clipboard as «class PNGf»',
                    "-e",
                    'set f to open for access POSIX file "/tmp/.clipboard-pty.png" with write permission',
                    "-e",
                    'set eof f to 0',
                    "-e",
                    'write png to f',
                    "-e",
                    'close access f',
                ],
                capture_output=True, timeout=3,
            )
            if result.returncode == 0 and os.path.exists("/tmp/.clipboard-pty.png"):
                with open("/tmp/.clipboard-pty.png", "rb") as fh:
                    data = fh.read()
                os.unlink("/tmp/.clipboard-pty.png")
                if data[:4] == b"\x89PNG":
                    return data
        except (subprocess.TimeoutExpired, OSError):
            pass
        return None

    # Linux: try wl-paste (Wayland), then xclip (X11)
    for cmd in (
        ["wl-paste", "-t", "image/png"],
        ["xclip", "-selection", "clipboard", "-t", "image/png", "-o"],
    ):
        if not shutil.which(cmd[0]):
            continue
        try:
            result = subprocess.run(cmd, capture_output=True, timeout=3)
            if result.returncode == 0 and result.stdout[:4] == b"\x89PNG":
                return result.stdout
        except (subprocess.TimeoutExpired, OSError):
            pass
    return None


def _save_clipboard(clipboard_dir):
    """Read clipboard image and save to clipboard_dir/clipboard.png."""
    data = _read_clipboard_image()
    if data:
        path = os.path.join(clipboard_dir, "clipboard.png")
        with open(path, "wb") as fh:
            fh.write(data)


def _resize_pty(child_fd):
    """Forward the current terminal size to the child pty."""
    try:
        sz = fcntl.ioctl(sys.stdin.fileno(), termios.TIOCGWINSZ, b"\x00" * 8)
        fcntl.ioctl(child_fd, termios.TIOCSWINSZ, sz)
    except OSError:
        pass


def main():
    if len(sys.argv) < 2:
        print("Usage: CLIPBOARD_DIR=/path python3 clipboard-pty.py <command> [args...]",
              file=sys.stderr)
        sys.exit(1)

    clipboard_dir = os.environ.get("CLIPBOARD_DIR", "")
    if not clipboard_dir:
        print("Error: CLIPBOARD_DIR not set", file=sys.stderr)
        sys.exit(1)

    os.makedirs(clipboard_dir, exist_ok=True)

    # Fork a child with a pty
    child_pid, child_fd = pty.fork()

    if child_pid == 0:
        # Child: exec the command
        os.execvp(sys.argv[1], sys.argv[1:])

    # Parent: relay I/O between terminal and child pty
    is_tty = os.isatty(sys.stdin.fileno())

    if is_tty:
        # Forward SIGWINCH to child
        def on_winch(signum, frame):
            _resize_pty(child_fd)
            os.kill(child_pid, signal.SIGWINCH)

        signal.signal(signal.SIGWINCH, on_winch)

        # Set initial size
        _resize_pty(child_fd)

    # Save and set raw mode on stdin (only when stdin is a real terminal)
    old_attrs = termios.tcgetattr(sys.stdin) if is_tty else None
    try:
        if is_tty:
            new_attrs = termios.tcgetattr(sys.stdin)
            # Enter raw mode: disable echo, canonical mode, signals, etc.
            new_attrs[0] = 0  # iflag
            new_attrs[1] = 0  # oflag
            new_attrs[2] &= ~(termios.CSIZE | termios.PARENB)  # cflag
            new_attrs[2] |= termios.CS8
            new_attrs[3] = 0  # lflag
            new_attrs[6][termios.VMIN] = 1
            new_attrs[6][termios.VTIME] = 0
            termios.tcsetattr(sys.stdin, termios.TCSAFLUSH, new_attrs)

        stdin_fd = sys.stdin.fileno()
        stdout_fd = sys.stdout.fileno()
        watch_fds = [stdin_fd, child_fd]

        while child_fd in watch_fds:
            try:
                rlist, _, _ = select.select(watch_fds, [], [])
            except (OSError, ValueError):
                break

            if stdin_fd in rlist:
                try:
                    data = os.read(stdin_fd, 1024)
                except OSError:
                    data = b""
                if not data:
                    watch_fds.remove(stdin_fd)
                else:
                    if CTRL_V in data:
                        _save_clipboard(clipboard_dir)
                    os.write(child_fd, data)

            if child_fd in rlist:
                try:
                    data = os.read(child_fd, 1024)
                except OSError:
                    break
                if not data:
                    break
                os.write(stdout_fd, data)

    finally:
        if old_attrs is not None:
            termios.tcsetattr(sys.stdin, termios.TCSAFLUSH, old_attrs)

    # Wait for child and exit with its status
    _, status = os.waitpid(child_pid, 0)
    sys.exit(os.waitstatus_to_exitcode(status))


if __name__ == "__main__":
    main()
