//! Terminal input for the in-process interactive search TUI.
//!
//! This module deliberately does NOT use `crossterm::event` on Unix. That
//! module lazily installs a process-wide SIGWINCH signal handler (via
//! signal-hook) the first time it is constructed, and the handler is never
//! unregistered. In a standalone `atuin` binary that is harmless — the process
//! exits. In an in-process library it would leave a function pointer into
//! `libatuin_ffi.so` behind after `dlclose`/`zmodload -u`, crashing the host
//! shell on the next terminal resize.
//!
//! Instead we open `/dev/tty` ourselves, switch it to raw mode with a
//! termios guard that restores the exact previous state on drop, and poll the
//! file descriptor directly. Resizes are picked up by re-querying the window
//! size on the regular poll timeout, so no signal handler is needed.
//!
//! On Windows `crossterm::event` reads console input records with
//! `WaitForMultipleObjects` — it installs no callbacks and holds no thread,
//! so it is safe to keep inside a loadable library.

/// A normalized key event used by the TUI state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Backspace,
    Delete,
    Tab,
    Enter,
    Esc,
    /// Ctrl + `a`..=`z`.
    Ctrl(char),
}

// ---------------------------------------------------------------------------
// Unix: /dev/tty + poll(2) + our own raw-mode guard
// ---------------------------------------------------------------------------

#[cfg(unix)]
pub mod unix {
    use super::Key;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read};
    use std::os::fd::{AsRawFd, RawFd};
    use std::time::Duration;

    /// Guard that restores the termios state saved when the TUI started.
    ///
    /// The shell's own line editor (zsh ZLE / PSReadLine) has already put the
    /// terminal in the mode it wants. We save exactly that state before
    /// switching to raw mode and restore exactly that state on drop — before
    /// the FFI call returns to the host editor.
    pub struct RawModeGuard {
        fd: RawFd,
        original: libc::termios,
    }

    impl RawModeGuard {
        pub fn enable(input: &Input) -> io::Result<Self> {
            // SAFETY: `original` is initialized by tcgetattr before use; the
            // fd is owned by `input`, which outlives the guard in the TUI.
            let mut original: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(input.file.as_raw_fd(), &mut original) } != 0 {
                return Err(io::Error::last_os_error());
            }

            let mut raw = original;
            // SAFETY: `raw` is a valid termios structure from tcgetattr.
            unsafe { libc::cfmakeraw(&mut raw) };
            if unsafe { libc::tcsetattr(input.file.as_raw_fd(), libc::TCSANOW, &raw) } != 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(Self {
                fd: input.file.as_raw_fd(),
                original,
            })
        }
    }

    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            // Best effort: the fd is still open at this point (the Input file
            // is dropped after the guard).
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
            }
        }
    }

    /// Non-blocking input source on the controlling terminal.
    pub struct Input {
        file: File,
        buf: Vec<u8>,
    }

    enum Parse {
        Key(Key, usize),
        NeedMore,
    }

    impl Input {
        pub fn open() -> io::Result<Self> {
            let file = OpenOptions::new().read(true).open("/dev/tty")?;

            // Non-blocking reads let us drain everything currently queued
            // without ever hanging on a partially-arrived escape sequence.
            // SAFETY: `file` owns the descriptor and the F_GETFL/F_SETFL
            // operations do not change ownership.
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }

            Ok(Self {
                file,
                buf: Vec::with_capacity(256),
            })
        }

        /// Wait up to `timeout` for input on the fd, or return immediately
        /// when previously read-but-unparsed bytes are buffered.
        pub fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
            if !self.buf.is_empty() {
                return Ok(true);
            }
            self.wait_fd(timeout)
        }

        /// Read the next normalized key, or `None` if no complete key is
        /// buffered. Callers must call [`Input::poll`] first and should treat
        /// `None` as "nothing available right now".
        pub fn read_key(&mut self) -> io::Result<Option<Key>> {
            loop {
                self.drain()?;
                if self.buf.is_empty() {
                    return Ok(None);
                }

                match parse(&self.buf) {
                    Parse::Key(key, len) => {
                        self.buf.drain(..len);
                        return Ok(Some(key));
                    }
                    Parse::NeedMore => {
                        // Give the rest of a multi-byte key (UTF-8 sequence or
                        // CSI escape) a short grace period to arrive. A bare
                        // ESC becomes Esc; a partial UTF-8 sequence is skipped.
                        if !self.wait_fd(Duration::from_millis(30))? {
                            let fallback = if self.buf[0] == 0x1b {
                                Key::Esc
                            } else {
                                Key::Char('\u{fffd}')
                            };
                            self.buf.drain(..1);
                            return Ok(Some(fallback));
                        }
                        // More bytes arrived; loop and try parsing again.
                    }
                }
            }
        }

        fn wait_fd(&mut self, timeout: Duration) -> io::Result<bool> {
            let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
            let mut pfd = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };

            loop {
                // SAFETY: pfd points at valid local memory and the fd is open.
                let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
                if rc >= 0 {
                    return Ok(rc > 0 && (pfd.revents & libc::POLLIN) != 0);
                }
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
        }

        fn drain(&mut self) -> io::Result<()> {
            let mut chunk = [0u8; 256];
            loop {
                match self.file.read(&mut chunk) {
                    Ok(0) => return Ok(()),
                    Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
        }
    }

    fn parse(buf: &[u8]) -> Parse {
        let first = buf[0];
        match first {
            0x1b => parse_escape(buf),
            0x08 | 0x7f => Parse::Key(Key::Backspace, 1),
            0x09 => Parse::Key(Key::Tab, 1),
            0x0a => Parse::Key(Key::Ctrl('j'), 1),
            0x0d => Parse::Key(Key::Enter, 1),
            0x20..=0x7e => Parse::Key(Key::Char(first as char), 1),
            0x01..=0x1a => {
                // Ctrl+A..Ctrl+Z. 0x08/0x09/0x0a/0x0d were handled above.
                let letter = char::from(b'a' + (first - 1));
                Parse::Key(Key::Ctrl(letter), 1)
            }
            0x80..=0xff => parse_utf8(buf),
            _ => Parse::Key(Key::Char('\u{fffd}'), 1),
        }
    }

    fn parse_escape(buf: &[u8]) -> Parse {
        debug_assert_eq!(buf[0], 0x1b);
        if buf.len() < 2 {
            return Parse::NeedMore;
        }

        match buf[1] {
            b'[' | b'O' => {
                // A CSI/SS3 sequence ends in a byte in 0x40..=0x7e. Cap the
                // scan so a malformed stream cannot grow the buffer forever.
                const MAX_SEQ: usize = 32;
                let Some(offset) = buf[2..]
                    .iter()
                    .take(MAX_SEQ)
                    .position(|&b| (0x40..=0x7e).contains(&b))
                else {
                    return if buf.len() >= 2 + MAX_SEQ {
                        Parse::Key(Key::Esc, 1)
                    } else {
                        Parse::NeedMore
                    };
                };

                let final_idx = offset + 2;
                let seq = &buf[2..final_idx];
                let key = match buf[final_idx] {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    b'C' => Key::Right,
                    b'D' => Key::Left,
                    b'H' => Key::Home,
                    b'F' => Key::End,
                    b'Z' => Key::Tab, // shift-tab
                    b'~' => {
                        let num = seq
                            .split(|&b| b == b';')
                            .next()
                            .and_then(|digits| std::str::from_utf8(digits).ok())
                            .and_then(|digits| digits.parse::<u32>().ok())
                            .unwrap_or(0);
                        match num {
                            1 | 7 => Key::Home,
                            3 => Key::Delete,
                            4 | 8 => Key::End,
                            5 => Key::PageUp,
                            6 => Key::PageDown,
                            _ => Key::Esc,
                        }
                    }
                    _ => Key::Esc,
                };
                Parse::Key(key, final_idx + 1)
            }
            // Alt-modified keys and other two-byte escapes are not used by
            // the TUI; consume just the ESC and let the next byte be parsed
            // on its own.
            _ => Parse::Key(Key::Esc, 1),
        }
    }

    fn parse_utf8(buf: &[u8]) -> Parse {
        let lead = buf[0];
        let expected = if lead >= 0xf0 {
            4
        } else if lead >= 0xe0 {
            3
        } else {
            2
        };

        if buf.len() < expected {
            return Parse::NeedMore;
        }

        match std::str::from_utf8(&buf[..expected]) {
            Ok(text) => {
                let ch = text.chars().next().expect("valid UTF-8 is non-empty");
                Parse::Key(Key::Char(ch), expected)
            }
            Err(_) => Parse::Key(Key::Char('\u{fffd}'), 1),
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: crossterm console input (callback-free)
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub mod windows {
    use super::Key;
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use std::io;
    use std::time::Duration;

    /// Windows raw-mode guard. crossterm saves/restores the console mode in a
    /// process-local static and installs no OS callbacks or threads, so this
    /// remains unload-safe.
    pub struct RawModeGuard;

    impl RawModeGuard {
        pub fn enable(_input: &Input) -> io::Result<Self> {
            ratatui::crossterm::terminal::enable_raw_mode()?;
            Ok(Self)
        }
    }

    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = ratatui::crossterm::terminal::disable_raw_mode();
        }
    }

    pub struct Input;

    impl Input {
        pub fn open() -> io::Result<Self> {
            Ok(Self)
        }

        pub fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
            event::poll(timeout)
        }

        pub fn read_key(&mut self) -> io::Result<Option<Key>> {
            match event::read()? {
                Event::Key(key) => Ok(map_key(key)),
                // Resize and other events are handled by the redraw loop.
                _ => Ok(None),
            }
        }
    }

    fn map_key(key: KeyEvent) -> Option<Key> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL)
            && let KeyCode::Char(c) = key.code
            && c.is_ascii_alphabetic()
        {
            return Some(Key::Ctrl(c.to_ascii_lowercase()));
        }

        let key = match key.code {
            KeyCode::Char(c) => Key::Char(c),
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Left => Key::Left,
            KeyCode::Right => Key::Right,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            KeyCode::PageUp => Key::PageUp,
            KeyCode::PageDown => Key::PageDown,
            KeyCode::Backspace => Key::Backspace,
            KeyCode::Delete => Key::Delete,
            KeyCode::Tab | KeyCode::BackTab => Key::Tab,
            KeyCode::Enter => Key::Enter,
            KeyCode::Esc => Key::Esc,
            _ => return None,
        };
        Some(key)
    }
}
