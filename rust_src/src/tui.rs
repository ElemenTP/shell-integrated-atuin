//! In-process interactive search TUI.
//!
//! This is the native replacement for the official `atuin search --interactive`
//! process spawn: it reuses the already-open `Session` (SQLite connections,
//! encryption key, settings) and renders a ratatui list on the controlling
//! terminal. zsh and PowerShell both call the same FFI entry point, so the UI
//! stays identical across shells and no `atuin` binary is needed.
//!
//! Terminal-discipline notes:
//! * output goes to stdout when it is a TTY, otherwise to `/dev/tty`
//!   (or `CONOUT$` on Windows) — mirroring the official TUI;
//! * input on Unix is read with our own poll/parse loop (see `tui_input`)
//!   so no SIGWINCH handler is installed that could outlive `dlclose`;
//! * raw mode and the alternate screen are RAII-restored, including on panic
//!   (the FFI `ffi_guard!` catches panics after the guards have dropped).

use atuin_client::database::DbSearchMode;
use atuin_client::history::History;
use atuin_client::session::Session;
use atuin_client::settings::SearchMode;
use eyre::{Result, WrapErr};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io::{IsTerminal, Write};
use std::time::Duration;

use crate::tui_input::Key;

#[cfg(unix)]
use crate::tui_input::unix;
#[cfg(windows)]
use crate::tui_input::windows;

/// Hard cap for live search results — matches the official TUI's default.
const RESULT_LIMIT: usize = 200;
/// Idle poll interval; also how often a terminal resize is noticed on Unix
/// (where we deliberately install no SIGWINCH handler).
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Run the interactive TUI and return the selected command.
///
/// `Ok(None)` means the user cancelled (Esc / Ctrl+C / Ctrl+G / Ctrl+D on an
/// empty query) and the caller must leave the shell buffer untouched.
/// `Ok(Some(command))` is either the selected history entry or — when there
/// are no matches — the query as typed, matching the official behavior.
pub fn interactive_search(session: &Session, initial_query: &str) -> Result<Option<String>> {
    let mut screen = ActiveScreen::new()?;
    let mut app = App::new(session, initial_query);
    app.refresh()?;

    let result = app.run(&mut screen)?;
    Ok(result)
}

// ---------------------------------------------------------------------------
// Terminal writer + screen/raw-mode RAII
// ---------------------------------------------------------------------------

/// Output target: stdout when it is a terminal, otherwise the controlling
/// terminal. This lets `atuin_search_interactive >file` still show the UI,
/// exactly like the official TUI does for command substitution.
enum TerminalWriter {
    Stdout(std::io::Stdout),
    #[cfg(unix)]
    Tty(std::fs::File),
    #[cfg(windows)]
    ConOut(std::io::LineWriter<std::fs::File>, u32),
}

impl TerminalWriter {
    fn new() -> std::io::Result<Self> {
        let stdout = std::io::stdout();
        if stdout.is_terminal() {
            return Ok(Self::Stdout(stdout));
        }

        #[cfg(unix)]
        {
            Ok(Self::Tty(
                std::fs::File::options()
                    .read(true)
                    .write(true)
                    .open("/dev/tty")?,
            ))
        }

        // Windows equivalent of /dev/tty. Set the console output code page to
        // UTF-8 for the duration of the TUI and restore it on drop.
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::{GetConsoleOutputCP, SetConsoleOutputCP};

            let file = std::fs::File::options()
                .read(true)
                .write(true)
                .open("CONOUT$")?;
            let previous_cp = unsafe { GetConsoleOutputCP() };
            const CP_UTF8: u32 = 65001;
            if previous_cp != CP_UTF8 {
                unsafe {
                    SetConsoleOutputCP(CP_UTF8);
                }
            }
            Ok(Self::ConOut(std::io::LineWriter::new(file), previous_cp))
        }

        #[cfg(not(any(unix, windows)))]
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "interactive search requires a terminal",
        ))
    }
}

impl Write for TerminalWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Stdout(stdout) => stdout.write(buf),
            #[cfg(unix)]
            Self::Tty(tty) => tty.write(buf),
            #[cfg(windows)]
            Self::ConOut(out, _) => out.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Stdout(stdout) => stdout.flush(),
            #[cfg(unix)]
            Self::Tty(tty) => tty.flush(),
            #[cfg(windows)]
            Self::ConOut(out, _) => out.flush(),
        }
    }
}

#[cfg(windows)]
impl Drop for TerminalWriter {
    fn drop(&mut self) {
        if let Self::ConOut(_, previous_cp) = self {
            use windows_sys::Win32::System::Console::SetConsoleOutputCP;
            if *previous_cp != 65001 {
                unsafe {
                    SetConsoleOutputCP(*previous_cp);
                }
            }
        }
    }
}

type ScreenBackend = CrosstermBackend<TerminalWriter>;

/// Holds the terminal, input source and raw-mode guard. Its Drop leaves the
/// alternate screen first, drops the ratatui terminal, and only then restores
/// the original termios/console mode.
struct ActiveScreen {
    terminal: Option<Terminal<ScreenBackend>>,
    /// Held purely for its Drop impl, which restores the original termios /
    /// console mode. Declared before `input` so field-drop order restores the
    /// terminal while the input fd is still open.
    _raw: RawModeGuard,
    input: Input,
}

#[cfg(unix)]
type Input = unix::Input;
#[cfg(windows)]
type Input = windows::Input;

#[cfg(unix)]
type RawModeGuard = unix::RawModeGuard;
#[cfg(windows)]
type RawModeGuard = windows::RawModeGuard;

impl ActiveScreen {
    fn new() -> Result<Self> {
        let input = Input::open()
            .map_err(|e| eyre::eyre!("cannot open terminal input: {e}"))
            .wrap_err("interactive search requires a controlling terminal")?;
        let raw = RawModeGuard::enable(&input)
            .map_err(|e| eyre::eyre!("cannot enable terminal raw mode: {e}"))?;

        let writer =
            TerminalWriter::new().map_err(|e| eyre::eyre!("cannot open terminal output: {e}"))?;
        let terminal = Terminal::new(CrosstermBackend::new(writer))?;

        let mut screen = Self {
            terminal: Some(terminal),
            _raw: raw,
            input,
        };
        let terminal = screen
            .terminal
            .as_mut()
            .expect("terminal initialized above");
        execute!(terminal.backend_mut(), EnterAlternateScreen)?;
        // Deliberately no `terminal.clear()` here: ratatui's clear() queries
        // the cursor position (ESC[6n) through crossterm's event module,
        // which on Unix would lazily install the SIGWINCH handler we avoid
        // for dlclose safety, and which plain pty test harnesses (no terminal
        // emulator) cannot answer. The first draw below paints the full frame
        // against the empty alternate-screen buffer anyway.
        Ok(screen)
    }
}

impl Drop for ActiveScreen {
    fn drop(&mut self) {
        if let Some(terminal) = self.terminal.as_mut() {
            // Leave the alternate screen while the backend writer is still
            // valid, then flush so the escape sequence reaches the terminal.
            let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
            let _ = terminal.backend_mut().flush();
        }
        self.terminal.take();
        // Field drop order then drops `_raw` before `input`, so the raw-mode
        // guard always restores termios/console mode while the fd is open.
    }
}

// ---------------------------------------------------------------------------
// TUI state machine
// ---------------------------------------------------------------------------

/// Result of processing one key.
enum Action {
    Continue,
    Accept { execute: bool },
    Cancel,
}

/// Editable query text with a UTF-8-safe cursor. Extracted from [`App`] so
/// the editing logic can be unit-tested without a database session.
struct QueryBuffer {
    text: String,
    /// Byte index of the cursor; always on a UTF-8 char boundary.
    cursor: usize,
}

impl QueryBuffer {
    fn new(initial: &str) -> Self {
        Self {
            text: initial.to_string(),
            cursor: initial.len(),
        }
    }

    fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.prev_char_boundary();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn delete_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let end = self.next_char_boundary();
        self.text.replace_range(self.cursor..end, "");
    }

    fn cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.prev_char_boundary();
        }
    }

    fn cursor_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.next_char_boundary();
        }
    }

    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    fn delete_word_before(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end();
        if trimmed.is_empty() {
            self.text.clear();
            self.cursor = 0;
            return;
        }
        let word_start = trimmed
            .rfind(char::is_whitespace)
            .map_or(0, |index| index + 1);
        // `trimmed` is a prefix of `before`, so byte offsets agree.
        self.text.replace_range(word_start..self.cursor, "");
        self.cursor = word_start;
    }

    fn prev_char_boundary(&self) -> usize {
        let before = &self.text[..self.cursor];
        before
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    fn next_char_boundary(&self) -> usize {
        let after = &self.text[self.cursor..];
        self.cursor + after.chars().next().map_or(0, char::len_utf8)
    }
}

struct App<'a> {
    session: &'a Session,
    input: QueryBuffer,
    selected: usize,
    mode: DbSearchMode,
    enter_accept: bool,
    results: Vec<History>,
    list_state: ListState,
}

impl<'a> App<'a> {
    fn new(session: &'a Session, initial_query: &str) -> Self {
        let mode = match session.settings.search_mode() {
            SearchMode::Prefix => DbSearchMode::Prefix,
            SearchMode::FullText => DbSearchMode::FullText,
            SearchMode::Fuzzy | SearchMode::DaemonFuzzy => DbSearchMode::Fuzzy,
        };

        Self {
            session,
            input: QueryBuffer::new(initial_query),
            selected: 0,
            mode,
            enter_accept: session.settings.enter_accept,
            results: Vec::new(),
            list_state: ListState::default(),
        }
    }

    fn refresh(&mut self) -> Result<()> {
        // The official TUI uses FullText for the empty-query "recent history"
        // view and the selected search mode once the user types something.
        let mode = if self.input.text.trim().is_empty() {
            DbSearchMode::FullText
        } else {
            self.mode
        };
        self.results = self.session.search(mode, &self.input.text, RESULT_LIMIT)?;
        self.selected = 0;
        Ok(())
    }

    fn run(mut self, screen: &mut ActiveScreen) -> Result<Option<String>> {
        loop {
            screen
                .terminal
                .as_mut()
                .expect("terminal is live while the TUI runs")
                .draw(|frame| self.render(frame))?;

            let input = &mut screen.input;
            if input.poll(POLL_INTERVAL)? {
                let Some(key) = input.read_key()? else {
                    // Resize / unknown event: redraw with the new size.
                    continue;
                };
                match self.handle_key(key)? {
                    Action::Continue => {}
                    Action::Accept { execute } => {
                        // With no matches, return the query as typed WITHOUT
                        // the __atuin_accept__: prefix — the official TUI
                        // never auto-executes a hand-typed query, only history
                        // selections.
                        let output =
                            accept_output(&self.results, self.selected, execute, &self.input.text);
                        return Ok(Some(output));
                    }
                    Action::Cancel => return Ok(None),
                }
            }
        }
    }

    fn handle_key(&mut self, key: Key) -> Result<Action> {
        match key {
            Key::Char(ch) => {
                self.input.insert_char(ch);
                self.after_query_change()?;
                Ok(Action::Continue)
            }
            Key::Backspace => {
                self.input.backspace();
                self.after_query_change()?;
                Ok(Action::Continue)
            }
            Key::Delete => {
                self.input.delete_forward();
                self.after_query_change()?;
                Ok(Action::Continue)
            }
            Key::Left | Key::Ctrl('b') => {
                self.input.cursor_left();
                Ok(Action::Continue)
            }
            Key::Right | Key::Ctrl('f') => {
                self.input.cursor_right();
                Ok(Action::Continue)
            }
            Key::Home | Key::Ctrl('a') => {
                self.input.cursor = 0;
                Ok(Action::Continue)
            }
            Key::End | Key::Ctrl('e') => {
                self.input.cursor = self.input.text.len();
                Ok(Action::Continue)
            }
            Key::Up | Key::Ctrl('p') => {
                self.select_previous();
                Ok(Action::Continue)
            }
            Key::Down | Key::Ctrl('n') | Key::Ctrl('j') => {
                self.select_next();
                Ok(Action::Continue)
            }
            Key::Ctrl('k') => {
                self.select_previous();
                Ok(Action::Continue)
            }
            Key::PageUp => {
                self.selected = self.selected.saturating_sub(10);
                Ok(Action::Continue)
            }
            Key::PageDown => {
                self.selected = self
                    .selected
                    .saturating_add(10)
                    .min(self.results.len().saturating_sub(1));
                Ok(Action::Continue)
            }
            Key::Ctrl('u') => {
                self.input.clear();
                self.after_query_change()?;
                Ok(Action::Continue)
            }
            Key::Ctrl('w') => {
                self.input.delete_word_before();
                self.after_query_change()?;
                Ok(Action::Continue)
            }
            Key::Ctrl('s') => {
                self.cycle_mode();
                self.refresh()?;
                Ok(Action::Continue)
            }
            Key::Ctrl('d') if self.input.text.is_empty() => Ok(Action::Cancel),
            Key::Ctrl('d') => {
                self.input.delete_forward();
                self.after_query_change()?;
                Ok(Action::Continue)
            }
            Key::Ctrl('l') => Ok(Action::Continue),
            Key::Esc | Key::Ctrl('c') | Key::Ctrl('g') => Ok(Action::Cancel),
            Key::Tab => Ok(Action::Accept { execute: false }),
            Key::Enter => Ok(Action::Accept {
                execute: self.enter_accept,
            }),
            // Unknown keys / unhandled Ctrl combos are ignored, matching the
            // official TUI's "continue" behavior for unbound keys.
            _ => Ok(Action::Continue),
        }
    }

    // -- editing ----------------------------------------------------------

    fn after_query_change(&mut self) -> Result<()> {
        self.selected = 0;
        self.refresh()
    }

    // -- selection / mode --------------------------------------------------

    fn select_next(&mut self) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1).min(self.results.len() - 1);
        }
    }

    fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn cycle_mode(&mut self) {
        self.mode = match self.mode {
            DbSearchMode::Fuzzy => DbSearchMode::Prefix,
            DbSearchMode::Prefix => DbSearchMode::FullText,
            DbSearchMode::FullText => DbSearchMode::Fuzzy,
        };
    }

    // -- rendering ---------------------------------------------------------

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 20 || area.height < 5 {
            let line = Line::from(Span::raw("terminal too small for atuin search"));
            frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);
            return;
        }

        let [header_area, list_area, footer_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(2),
            Constraint::Length(1),
        ])
        .areas(area);

        // -- query / header -------------------------------------------------
        let (before, after) = self.input.text.split_at(self.input.cursor);
        let query_line = Line::from(vec![
            Span::raw("> "),
            Span::raw(before),
            Span::styled("▌", Style::default().add_modifier(Modifier::REVERSED)),
            Span::raw(after),
        ]);
        let mode_name = match self.mode {
            DbSearchMode::Fuzzy => "fuzzy",
            DbSearchMode::Prefix => "prefix",
            DbSearchMode::FullText => "full-text",
        };
        let header = Paragraph::new(query_line)
            .block(Block::default().borders(Borders::ALL).title(format!(
                " Atuin (native) · {mode_name} · {} matches ",
                self.results.len()
            )))
            .wrap(Wrap { trim: false });
        frame.render_widget(header, header_area);

        // -- results --------------------------------------------------------
        let items: Vec<ListItem> = if self.results.is_empty() {
            vec![ListItem::new(Span::styled(
                "(no matches — Enter accepts the query as typed)",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            self.results
                .iter()
                .map(|history| ListItem::new(display_command(&history.command)))
                .collect()
        };
        let selected = if self.results.is_empty() {
            None
        } else {
            Some(self.selected.min(self.results.len() - 1))
        };
        self.list_state.select(selected);

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Results "))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, list_area, &mut self.list_state);

        // -- footer ---------------------------------------------------------
        let enter_hint = if self.enter_accept {
            "enter=run"
        } else {
            "enter=select"
        };
        let footer = Paragraph::new(Line::from(Span::styled(
            format!(
                " {enter_hint}  tab=select (no run)  esc=cancel  ctrl+s=search mode  ↑/↓=move "
            ),
            Style::default().fg(Color::DarkGray),
        )))
        .alignment(Alignment::Center);
        frame.render_widget(footer, footer_area);
    }
}

/// Compute the string returned to the shell when the user accepts.
///
/// A selected history entry may carry the `__atuin_accept__:` execute prefix;
/// a hand-typed query never does, matching the official TUI.
fn accept_output(results: &[History], selected: usize, execute: bool, query: &str) -> String {
    match results.get(selected) {
        Some(history) if execute => format!("__atuin_accept__:{}", history.command),
        Some(history) => history.command.clone(),
        None => query.to_string(),
    }
}

/// Render a history command on a single display line.
fn display_command(command: &str) -> String {
    let mut display = String::with_capacity(command.len());
    for ch in command.chars() {
        match ch {
            '\n' => display.push('⏎'),
            '\r' => {}
            ch if ch.is_control() => display.push('?'),
            ch => display.push(ch),
        }
    }
    if display.is_empty() {
        display.push(' ');
    }
    display
}

// ---------------------------------------------------------------------------
// Tests (pure state-machine / rendering behavior, no terminal required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui_input::Key;

    #[test]
    fn test_display_command_collapses_newlines_and_controls() {
        assert_eq!(display_command("echo hello"), "echo hello");
        assert_eq!(display_command("echo a\nb"), "echo a⏎b");
        assert_eq!(display_command("a\tb"), "a?b");
        assert_eq!(display_command(""), " ");
        assert_eq!(display_command("\r\n"), "⏎");
    }

    #[test]
    fn test_editing_keeps_cursor_on_char_boundaries() {
        let mut state = QueryBuffer::new("héllo");
        state.cursor_left();
        state.cursor_left();
        state.insert_char('!');
        assert_eq!(state.text, "hél!lo");
        state.backspace();
        assert_eq!(state.text, "héllo");

        state.cursor_left();
        state.cursor_left();
        state.insert_char('界');
        assert_eq!(state.text, "h界éllo");
        state.backspace();
        assert_eq!(state.text, "héllo");
        state.backspace();
        assert_eq!(state.text, "éllo");
        assert!(state.text.is_char_boundary(state.cursor));
    }

    #[test]
    fn test_editing_empty_input_is_stable() {
        let mut state = QueryBuffer::new("");
        state.backspace();
        state.cursor_left();
        state.insert_char('x');
        state.backspace();
        state.backspace();
        assert_eq!(state.text, "");
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn test_delete_word_before() {
        let mut state = QueryBuffer::new("echo one two ");
        state.delete_word_before();
        assert_eq!(state.text, "echo one ");
        state.delete_word_before();
        assert_eq!(state.text, "echo ");
        state.delete_word_before();
        assert_eq!(state.text, "");
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn test_cursor_never_splits_utf8() {
        let mut state = QueryBuffer::new("a界b");
        state.cursor_left();
        state.cursor_left();
        state.cursor_left();
        state.insert_char('-');
        assert_eq!(state.text, "-a界b");
        assert!(state.text.is_char_boundary(state.cursor));
    }

    #[test]
    fn test_key_variants_are_comparable() {
        assert_eq!(Key::Ctrl('c'), Key::Ctrl('c'));
        assert_ne!(Key::Ctrl('c'), Key::Char('c'));
        assert_ne!(Key::Esc, Key::Ctrl('c'));
    }

    #[test]
    fn test_accept_output_matches_official_semantics() {
        let history = |command: &str| {
            History::capture()
                .timestamp(time::OffsetDateTime::now_utc())
                .command(command)
                .cwd("/tmp")
                .build()
                .into()
        };
        let results = vec![history("echo newest"), history("echo oldest")];

        assert_eq!(
            accept_output(&results, 0, true, "echo"),
            "__atuin_accept__:echo newest"
        );
        assert_eq!(accept_output(&results, 0, false, "echo"), "echo newest");
        assert_eq!(
            accept_output(&results, 1, true, "echo"),
            "__atuin_accept__:echo oldest"
        );
        // No matches: never auto-execute the hand-typed query.
        assert_eq!(accept_output(&[], 0, true, "echo typed"), "echo typed");
        assert_eq!(accept_output(&[], 0, false, "echo typed"), "echo typed");
    }

    #[test]
    fn test_cycle_mode_order_matches_ctrl_s() {
        let mut mode = DbSearchMode::Fuzzy;
        let next = |mode: DbSearchMode| match mode {
            DbSearchMode::Fuzzy => DbSearchMode::Prefix,
            DbSearchMode::Prefix => DbSearchMode::FullText,
            DbSearchMode::FullText => DbSearchMode::Fuzzy,
        };
        mode = next(mode);
        assert_eq!(mode, DbSearchMode::Prefix);
        mode = next(mode);
        assert_eq!(mode, DbSearchMode::FullText);
        mode = next(mode);
        assert_eq!(mode, DbSearchMode::Fuzzy);
    }
}
