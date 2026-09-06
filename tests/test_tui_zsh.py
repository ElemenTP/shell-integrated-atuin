#!/usr/bin/env python3
"""Integration test for the in-process interactive TUI search (zsh).

Runs a real zsh under a pseudo-terminal, loads the atuin_native module,
records commands, and drives the full-screen TUI builtin with real key
strokes. It verifies:

  * the TUI enters/leaves the alternate screen;
  * Enter selects the newest match;
  * ArrowUp + Enter selects the older match (upstream default);
  * Escape cancels and leaves $ATUIN_SEARCH_SELECTED empty;
  * terminal raw mode is restored (the shell still echoes/executes commands);
  * the plugin's atuin-search ZLE widget (Ctrl+R) drives the same TUI and
    replaces the line editor buffer with the selection.

Usage:
  MODULE_DIR=/path/to/zsh_src/build REPO_ROOT=/path/to/repo \
      python3 tests/test_tui_zsh.py

  PLUGIN may point at an installed copy of atuin-native.plugin.zsh
  (default: $REPO_ROOT/zsh_src/atuin-native.plugin.zsh).
"""

import os
import select
import shutil
import sys
import tempfile
import time


class PtyShell:
    def __init__(self, module_dir: str, repo_root: str, data_dir: str):
        import fcntl
        import pty
        import termios
        import struct

        env = os.environ.copy()
        env.update({
            "TERM": "xterm-256color",
            "ATUIN_DATA_DIR": data_dir,
            "MODULE_DIR": module_dir,
            "REPO_ROOT": repo_root,
        })

        # pty.fork() makes the child a session leader with the pty as its
        # controlling terminal, which the TUI needs for /dev/tty.
        pid, self.master = pty.fork()
        if pid == 0:
            os.execvpe("zsh", ["zsh", "-f"], env)

        self.pid = pid
        # A sane, fixed terminal size makes the TUI layout deterministic.
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        self.buf = b""

    def send(self, text: str) -> None:
        os.write(self.master, text.encode("utf-8"))

    def read(self, timeout: float = 1.0) -> bytes:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            ready, _, _ = select.select([self.master], [], [], 0.05)
            if ready:
                try:
                    chunk = os.read(self.master, 65536)
                except OSError:
                    return b""
                if not chunk:
                    return b""
                self.buf += chunk
        return self.buf

    def wait_for(self, needle: bytes, timeout: float = 10.0) -> bytes:
        """Wait until `needle` appears; return everything read so far."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if needle in self.buf:
                return self.buf
            self.read(0.2)
        raise AssertionError(
            f"timed out waiting for {needle!r}.\n"
            f"--- captured output ---\n{self.buf.decode('utf-8', 'replace')}"
        )

    def run(self, command: str, marker: str, timeout: float = 10.0) -> None:
        """Run one shell command and wait for its unique output marker."""
        self.send(command.rstrip("\n") + "\n")
        self.wait_for(marker.encode(), timeout)

    def close(self) -> None:
        try:
            self.send("exit\n")
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                waited, _ = os.waitpid(self.pid, os.WNOHANG)
                if waited == self.pid:
                    break
                time.sleep(0.05)
            else:
                os.kill(self.pid, 9)
                os.waitpid(self.pid, 0)
        except (OSError, ChildProcessError):
            pass
        finally:
            os.close(self.master)


def wait_for_alt_screen(shell: PtyShell, entered: bool) -> bytes:
    """Wait for the TUI to enter (1049h) or leave (1049l) the alt screen."""
    needle = b"\x1b[?1049h" if entered else b"\x1b[?1049l"
    out = shell.wait_for(needle, timeout=10.0)
    time.sleep(0.25)  # let ratatui paint the first frame
    return out


def main() -> int:
    module_dir = os.environ.get("MODULE_DIR", "zsh_src/build")
    repo_root = os.environ.get("REPO_ROOT", os.getcwd())
    plugin = os.environ.get(
        "PLUGIN", os.path.join(repo_root, "zsh_src", "atuin-native.plugin.zsh")
    )
    if not os.path.isfile(os.path.join(module_dir, "atuin_native.so")):
        print(f"FAIL: atuin_native.so not found in MODULE_DIR={module_dir}")
        return 1

    data_dir = tempfile.mkdtemp(prefix="atuin-tui-test-")
    shell = PtyShell(module_dir, repo_root, data_dir)
    tests = 0
    try:
        # ---- Boot the module -------------------------------------------------
        shell.run(
            "module_path=($MODULE_DIR $module_path); "
            "zmodload atuin_native || print -u2 LOAD_FAILED; "
            "print -r -- TUI_BOOT_OK",
            "TUI_BOOT_OK",
        )
        tests += 1

        # ---- Export the env vars the upstream TUI expects -------------------
        # The real plugin does this before search; the direct-builtin tests need
        # the same environment because the upstream interactive search calls
        # current_context() and reads ATUIN_SHELL for the accept prefix.
        shell.run(
            "atuin_session_id >/dev/null; "
            "export ATUIN_SESSION; "
            "export ATUIN_SHELL=zsh; "
            "print -r -- TUI_ENV_OK",
            "TUI_ENV_OK",
        )
        tests += 1

        # ---- Seed history ----------------------------------------------------
        shell.run(
            "atuin_history_start 'echo tui-oldest' \"$PWD\" >/dev/null 2>&1; "
            "atuin_history_end \"${ATUIN_HISTORY_ID:-}\" 0 0 --sync; "
            "ATUIN_HISTORY_ID=''; "
            "atuin_history_start 'echo tui-newest' \"$PWD\" >/dev/null 2>&1; "
            "atuin_history_end \"${ATUIN_HISTORY_ID:-}\" 0 0 --sync; "
            "print -r -- TUI_SEED_OK",
            "TUI_SEED_OK",
        )
        tests += 1

        # ---- 1. Enter selects the newest match -------------------------------
        shell.send('atuin_search_interactive "echo tui"\n')
        wait_for_alt_screen(shell, entered=True)
        shell.send("\r")  # Enter: newest first
        wait_for_alt_screen(shell, entered=False)
        shell.run(
            'print -r -- "TUI_SEL:${ATUIN_SEARCH_SELECTED:-}"',
            "TUI_SEL:__atuin_accept__:echo tui-newest",
        )
        tests += 1

        # ---- 2. ArrowUp + Enter selects the older match --------------------
        shell.send('atuin_search_interactive "echo tui"\n')
        wait_for_alt_screen(shell, entered=True)
        shell.send("\x1b[A")  # ArrowUp
        shell.send("\r")
        wait_for_alt_screen(shell, entered=False)
        shell.run(
            'print -r -- "TUI_SEL2:${ATUIN_SEARCH_SELECTED:-}"',
            "TUI_SEL2:__atuin_accept__:echo tui-oldest",
        )
        tests += 1

        # ---- 3. Escape cancels and keeps the parameter empty -----------------
        shell.send('atuin_search_interactive "no-such-command-zz"\n')
        wait_for_alt_screen(shell, entered=True)
        shell.send("\x1b")  # Esc
        wait_for_alt_screen(shell, entered=False)
        shell.run(
            'print -r -- "TUI_CANCEL:${ATUIN_SEARCH_SELECTED:-}:RC:$?"',
            "TUI_CANCEL::RC:1",
        )
        tests += 1

        # ---- 4. The plugin widget (Ctrl+R in ZLE) drives the same TUI --------
        shell.run(
            f"zmodload -u atuin_native; "
            f"export ATUIN_NATIVE_DIR='{module_dir}'; "
            f"source '{plugin}'; "
            "print -r -- TUI_WIDGET_OK",
            "TUI_WIDGET_OK",
        )
        tests += 1

        # The plugin must install the official default key bindings itself
        # (`atuin init zsh` behavior); Ctrl+R below must work without any
        # manual bindkey here.
        shell.run("bindkey -M emacs '^r'", "atuin-search")
        tests += 1

        shell.send("echo tui")          # type into the ZLE buffer
        time.sleep(0.3)
        shell.send("\x12")              # Ctrl+R -> atuin-search widget
        wait_for_alt_screen(shell, entered=True)
        shell.send("\r")                # select newest; enter_accept=true makes the
                                         # widget strip __atuin_accept__: and call
                                         # zle accept-line, so the command executes
                                         # without a second Enter.
        wait_for_alt_screen(shell, entered=False)
        shell.wait_for(b"tui-newest", timeout=10.0)
        tests += 1

        # ---- 5. UpArrow binding opens the TUI; Esc keeps the buffer -------
        shell.send("echo tui")
        time.sleep(0.3)
        shell.send("\x1b[A")           # UpArrow (CSI form)
        wait_for_alt_screen(shell, entered=True)
        shell.send("\x1b")             # Esc cancels; buffer must stay intact
        wait_for_alt_screen(shell, entered=False)
        shell.send("\r")               # execute the untouched original buffer
        shell.wait_for(b"\r\ntui\r\n", timeout=10.0)
        tests += 1

        # ---- 6. Module unload after TUI widget use -------------------------
        shell.run(
            "zmodload -u atuin_native; print -r -- TUI_ZSH_UNLOADED",
            "TUI_ZSH_UNLOADED",
        )
        tests += 1

        print(f"=== All atuin TUI tests passed ({tests} checks) ===")
        return 0
    except Exception as exc:  # noqa: BLE001 - integration test diagnostics
        print(f"FAIL: {exc}")
        print("--- last output ---")
        print(shell.buf.decode("utf-8", "replace"))
        return 1
    finally:
        shell.close()
        shutil.rmtree(data_dir, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
