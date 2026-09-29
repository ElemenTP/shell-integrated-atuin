#!/usr/bin/env python3
"""Integration test for the in-process interactive TUI search (PowerShell).

Runs pwsh under a pseudo-terminal and verifies the same native TUI that the
zsh test drives:

  * the managed AtuinSession.SearchInteractive P/Invoke opens the TUI and
    returns the newest match (with the __atuin_accept__: prefix because
    enter_accept defaults to true);
  * Escape cancels and returns null;
  * the module's Ctrl+R PSReadLine key handler opens the TUI in-process and
    accepts the selected command directly into the prompt.

A tiny terminal-emulator responder answers PSReadLine's startup cursor-position
query (ESC[6n); without it pwsh waits for the query response forever.

Usage:
  DLL_DIR=/path/to/pwsh_src/AtuinNative/bin/Release/net8.0 \
      python3 tests/test_tui_pwsh.py
"""

import fcntl
import os
import pty
import re
import select
import shutil
import struct
import sys
import tempfile
import termios
import threading
import time

ALT_ENTER = b"\x1b[?1049h"
ALT_LEAVE = b"\x1b[?1049l"


class PwshPty:
    def __init__(self, dll_dir: str, data_dir: str):
        env = os.environ.copy()
        # atuin_init takes no data directory, so the session resolves
        # its paths from the settings tree. Pass an isolated settings tree to
        # the child through the real environment (execvpe), which the native
        # Rust code reads.
        env.update(
            TERM="xterm-256color",
            ATUIN_CONFIG_DIR=os.path.join(data_dir, "config"),
            ATUIN_DATA_DIR=data_dir,
        )

        pwsh = os.environ.get("PWSH", "pwsh")
        pid, self.master = pty.fork()
        if pid == 0:
            os.execvpe(pwsh, [pwsh, "-NoProfile"], env)
        self.pid = pid

        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        self.dll_dir = dll_dir
        self.data_dir = data_dir
        self.buf = b""
        self._stop = False
        # pwsh 7.6 asks the terminal for its cursor position (ESC[6n) while
        # PSReadLine starts. A pty is not a terminal emulator, so answer it.
        self._thread = threading.Thread(target=self._responder, daemon=True)
        self._thread.start()

    def _responder(self) -> None:
        while not self._stop:
            ready, _, _ = select.select([self.master], [], [], 0.1)
            if not ready:
                continue
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                return
            if not chunk:
                return
            self.buf += chunk
            if b"\x1b[6n" in chunk:
                os.write(self.master, b"\x1b[1;1R")

    def send(self, text: str) -> None:
        os.write(self.master, text.encode("utf-8"))

    def wait_for(self, needle: bytes, timeout: float = 20.0) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if needle in self.buf:
                return
            time.sleep(0.1)
        raise AssertionError(
            f"timed out waiting for {needle!r}.\n"
            f"--- captured output (plain) ---\n{self.plain()[-4000:]}"
        )

    def run_command(self, command: str, output_marker: bytes, timeout: float = 20.0) -> None:
        # pwsh in application-cursor-keys mode executes on \r, not \n.
        self.send(command + "\r")
        self.wait_for(output_marker, timeout)

    def plain(self) -> bytes:
        """Strip the ANSI sequences PSReadLine interleaves into echoed text."""
        text = re.sub(rb"\x1b\][^\x07]*\x07", b"", self.buf)
        text = re.sub(rb"\x1b\[[0-9;?]*[ -/]*[@-~]", b"", text)
        return re.sub(rb"\x1b[=>]", b"", text)

    def close(self) -> None:
        try:
            self._stop = True
            self._thread.join(timeout=1)
            os.kill(self.pid, 9)
        except ProcessLookupError:
            pass
        try:
            os.waitpid(self.pid, 0)
        except ChildProcessError:
            pass
        os.close(self.master)


def check_readline_wrapper(dll_dir: str) -> bool:
    """Regression check for the fresh-shell PSConsoleHostReadLine wrapper.

    The module applies Set-StrictMode -Version Latest. Reading $LASTEXITCODE
    before strict mode was disabled aborted the wrapper on the first prompt of
    a fresh shell (no native command had run yet), which silently disabled
    history recording when $ErrorActionPreference was 'Stop'. Run the real
    wrapper in exactly that state and verify the session recorded the command.
    """
    data_dir = tempfile.mkdtemp(prefix="atuin-pwsh-readline-")
    config_dir = os.path.join(data_dir, "config")
    os.makedirs(config_dir, exist_ok=True)
    with open(os.path.join(config_dir, "config.toml"), "w", encoding="utf-8") as handle:
        handle.write(f'data_dir = "{data_dir}"\n')

    shell = PwshPty(dll_dir, data_dir)
    try:
        shell.wait_for(b"> ", timeout=20)
        time.sleep(0.5)
        shell.run_command(
            "$ErrorActionPreference='Stop'; "
            "Write-Output ('NOSET:'+[string](Test-Path variable:global:LASTEXITCODE))",
            b"NOSET:False",
        )
        shell.run_command(
            f"Import-Module '{dll_dir}/atuin-native.psd1' -Force; Write-Output 'IMPORTED'",
            b"IMPORTED",
        )
        time.sleep(0.3)
        shell.send("echo wrapper-probe\r")
        time.sleep(1.0)
        shell.send(
            "Write-Output ('WRAPPEROK:'+[string]"
            "([AtuinNative.Session]::GetStats().HistoryStarts -ge 1))\r"
        )
        shell.wait_for(b"WRAPPEROK:True", timeout=10)
        return True
    except Exception:  # noqa: BLE001 - caller reports the failure
        print("--- fresh-shell wrapper output (plain) ---")
        print(shell.plain()[-2000:].decode("utf-8", "replace"))
        return False
    finally:
        shell.close()
        shutil.rmtree(data_dir, ignore_errors=True)


def main() -> int:
    dll_dir = os.environ.get("DLL_DIR")
    if not dll_dir:
        print("FAIL: DLL_DIR must point at the built AtuinNative output directory")
        return 1

    if sys.platform == "win32":
        native_lib = "atuin_ffi.dll"
    elif sys.platform == "darwin":
        native_lib = "libatuin_ffi.dylib"
    else:
        native_lib = "libatuin_ffi.so"

    required = (
        os.path.join(dll_dir, "AtuinNative.dll"),
        os.path.join(dll_dir, "atuin-native.psd1"),
        os.path.join(dll_dir, native_lib),
    )
    if not all(os.path.isfile(path) for path in required):
        print(f"FAIL: expected module files not found in {dll_dir}: {required}")
        return 1

    # ---- 0. Fresh-shell PSConsoleHostReadLine wrapper (strict-mode regression)
    if not check_readline_wrapper(dll_dir):
        print("FAIL: PSConsoleHostReadLine wrapper did not record history in a fresh shell")
        return 1

    data_dir = tempfile.mkdtemp(prefix="atuin-pwsh-tui-")
    config_dir = os.path.join(data_dir, "config")
    os.makedirs(config_dir, exist_ok=True)
    with open(os.path.join(config_dir, "config.toml"), "w", encoding="utf-8") as handle:
        # Mirror the shipped default config for the settings the TUI test relies
        # on: enter_accept=true is the upstream default and gives results the
        # __atuin_accept__: prefix.
        handle.write(f'data_dir = "{data_dir}"\nenter_accept = true\n')
    shell = PwshPty(dll_dir, data_dir)
    checks = 0
    try:
        shell.wait_for(b"> ", timeout=20)
        time.sleep(0.8)

        # ---- Direct managed session + seed history --------------------------
        # Intentionally do NOT import the .psm1 yet: its PSConsoleHostReadLine
        # wrapper records every executed command, so a direct SearchInteractive
        # invocation would otherwise match its own command line as the newest
        # history entry.
        shell.run_command(
            f"$env:ATUIN_FFI_PATH='{dll_dir}/libatuin_ffi.so'; "
            f"Add-Type -Path '{dll_dir}/AtuinNative.dll'; "
            "[AtuinNative.AtuinEnvironment]::Set('ATUIN_SHELL','powershell'); "
            "[AtuinNative.Session]::Initialize(); "
            "[AtuinNative.AtuinEnvironment]::Set('ATUIN_SESSION',[AtuinNative.Session]::SessionUuid()); "
            "$id=[AtuinNative.Session]::HistoryStart('echo pwsh-tui-oldest', (Get-Location).Path); "
            "[AtuinNative.Session]::HistoryEnd($id,0,0,$true); "
            "$id=[AtuinNative.Session]::HistoryStart('echo pwsh-tui-newest', (Get-Location).Path); "
            "[AtuinNative.Session]::HistoryEnd($id,0,0,$true); "
            "Write-Output 'TUI_PWSH_SEED_OK'",
            b"TUI_PWSH_SEED_OK\r\n",
        )
        checks += 1

        # ---- 1. Managed SearchInteractive selects the newest match ---------
        shell.run_command("$sel=[AtuinNative.Session]::SearchInteractive('echo pwsh')", b"")  # starts the TUI
        shell.wait_for(ALT_ENTER)
        time.sleep(0.3)
        shell.send("\r")
        shell.wait_for(ALT_LEAVE)
        time.sleep(0.5)  # let PSReadLine settle back on the main screen
        shell.run_command("Write-Output ('SEL:'+$sel)", b"SEL:__atuin_accept__:echo pwsh-tui-newest\r\n")
        checks += 1

        # ---- 2. Escape cancels and returns null ----------------------------
        shell.run_command("$cancel=[AtuinNative.Session]::SearchInteractive('zz-no-match')", b"")
        shell.wait_for(ALT_ENTER)
        time.sleep(0.3)
        shell.send("\x1b")
        shell.wait_for(ALT_LEAVE)
        time.sleep(0.5)
        shell.run_command("Write-Output ('CANCEL:'+[string]$cancel)", b"CANCEL:\r\n")
        checks += 1

        # ---- 3. Ctrl+R PSReadLine handler runs the TUI in-process ----------
        # Shut the direct session down and let the module create its own. The
        # module must auto-bind Ctrl+R / UpArrow on import exactly like
        # `atuin init powershell` — no explicit Enable-AtuinSearchKeys here.
        shell.run_command(
            "[AtuinNative.Session]::Shutdown(); "
            f"Import-Module '{dll_dir}/atuin-native.psd1' -Force; "
            "Write-Output 'TUI_PWSH_IMPORT_OK'",
            b"TUI_PWSH_IMPORT_OK\r\n",
        )
        time.sleep(0.3)
        shell.send("echo pwsh")
        time.sleep(0.5)
        shell.send("\x12")  # Ctrl+R
        shell.wait_for(ALT_ENTER)
        time.sleep(0.3)
        shell.send("\r")  # Enter; enter_accept=true executes the selection
        shell.wait_for(ALT_LEAVE)
        shell.wait_for(b"pwsh-tui-newest\r\n")
        time.sleep(0.5)
        checks += 1

        # ---- 4. UpArrow auto-binding opens the TUI; Esc keeps buffer --------
        shell.send("echo pwsh")
        time.sleep(0.5)
        shell.send("\x1bOA")            # UpArrow (application cursor mode)
        shell.wait_for(ALT_ENTER)
        time.sleep(0.3)
        shell.send("\x1b")              # Esc cancels; buffer must stay intact
        shell.wait_for(ALT_LEAVE)
        time.sleep(0.5)
        shell.send("\r")                # execute the untouched original buffer
        shell.wait_for(b"pwsh\r\n")
        checks += 1

        # ---- 5. Module cleanup after TUI use --------------------------------
        shell.run_command(
            "Remove-Module atuin-native -Force; Write-Output 'TUI_PWSH_REMOVED'",
            b"TUI_PWSH_REMOVED\r\n",
        )
        checks += 1

        print(f"=== All pwsh TUI tests passed ({checks} checks) ===")
        return 0
    except Exception as exc:  # noqa: BLE001 - integration test diagnostics
        print(f"FAIL: {exc}")
        print("--- last output (plain) ---")
        print(shell.plain()[-4000:].decode("utf-8", "replace"))
        return 1
    finally:
        shell.close()
        shutil.rmtree(data_dir, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
