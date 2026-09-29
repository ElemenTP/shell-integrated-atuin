//! C FFI exports for Atuin in-process history recording and search.
//!
//! All public functions use the `atuin_` prefix and the C ABI.
//!
//! # Error protocol
//!
//! Every fallible `atuin_*` export returns `*mut c_char`:
//!
//! * `NULL` means success.
//! * A non-NULL value is a heap-allocated, NUL-terminated UTF-8 error string.
//!   The caller owns it and must release it with [`atuin_free`].
//!
//! This deliberately eliminates all process-global / per-session error slots.
//! Each call carries its own error value directly in its return value, so there
//! is no shared mutable state to race on and no TLS destructor to dangle after
//! `dlclose()`. [`atuin_free`] itself cannot fail and therefore returns `void`;
//! [`atuin_version`] returns a static string rather than an error.
//!
//! # Single session
//!
//! Atuin's client layer keeps process-global state (the resolved data directory
//! and the meta store) that is only valid for one settings/session instance, so
//! this library exposes **at most one session per loaded library instance**:
//!
//! * [`atuin_init`] is idempotent: calling it while a session is already active
//!   succeeds and keeps the existing session.
//! * [`atuin_shutdown`] is idempotent and must be called before the
//!   library is unloaded so the tokio/sqlx workers and the meta store are shut
//!   down.
//! * Every other export operates on the active session and fails with
//!   `session is not initialized` when there is none.
//! * Calls are serialized by one lock; `atuin_search_interactive` holds it for
//!   the whole TUI, which matches the official blocking `atuin search -i`.
//!
//! Functions that produce a value write it through an out parameter:
//!
//! | Function | Output parameter | Free with |
//! |----------|------------------|-----------|
//! | `atuin_history_start` | `*id_out` history ID | `atuin_free` |
//! | `atuin_search` / `atuin_search_prefix` | `*out` newline-separated commands | `atuin_free` |
//! | `atuin_search_interactive` | `*out` selection, or NULL on cancel | `atuin_free` |
//! | `atuin_session_uuid` | `*out` session-owned UUID | must NOT be freed |
//! | `atuin_stats` | `*out` session counter snapshot | must NOT be freed |
//! | `atuin_version` | returned static string | must NOT be freed |

use libc::c_char;
use std::ffi::{CStr, CString};
use std::os::raw::c_int;
use std::path::PathBuf;
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use atuin::session::{SearchMode, SearchOptions, SearchResult, SessionStats};
use atuin_client::history::{AuthorKind, AuthorPattern};
use atuin_client::settings::{FilterMode, KeymapMode, RequestedSearchMode};

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

/// Copy a Rust string into a C-owned NUL-terminated UTF-8 buffer.
///
/// Interior NUL bytes are truncated (history/error data never contains NUL in
/// practice, but the FFI boundary must stay total).
fn string_into_c(value: String) -> *mut c_char {
    CString::new(value)
        .unwrap_or_else(|e| {
            let pos = e.nul_position();
            let mut bytes = e.into_vec();
            bytes.truncate(pos);
            CString::new(bytes).unwrap()
        })
        .into_raw()
}

/// Build an allocated error string for `msg`.
fn error_string(msg: impl Into<String>) -> *mut c_char {
    string_into_c(msg.into())
}

/// Convert a caught panic into an allocated error string.
fn panic_to_error(panic: Box<dyn std::any::Any + Send>) -> *mut c_char {
    let msg = if let Some(s) = panic.downcast_ref::<&str>() {
        format!("panic: {s}")
    } else if let Some(s) = panic.downcast_ref::<String>() {
        format!("panic: {s}")
    } else {
        "panic: unknown error".to_string()
    };
    string_into_c(msg)
}

/// FFI panic guard for fallible exports.
///
/// The wrapped expression must itself return `*mut c_char` using the error
/// protocol above. Panics are caught and converted to an allocated error
/// string instead of unwinding across the C boundary.
macro_rules! ffi_guard_error {
    ($expr:expr) => {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $expr)) {
            Ok(result) => result,
            Err(panic) => panic_to_error(panic),
        }
    };
}

// ---------------------------------------------------------------------------
// Global session
// ---------------------------------------------------------------------------

/// The single process-wide session.
///
/// `None` means "no session": either never created or already shut down. The
/// stored `CString` is what `atuin_session_uuid` hands out, so its address
/// changes when the session is recreated.
static SESSION: Mutex<Option<SessionHandle>> = Mutex::new(None);

/// PID of the process that created `SESSION` (0 when no session exists).
///
/// Kept outside the Mutex so a forked child can reject FFI calls without
/// touching a mutex that may have been held when `fork()` happened.
static SESSION_PID: AtomicU32 = AtomicU32::new(0);

/// Owned session state (opaque to C; there is no C handle any more).
struct SessionHandle {
    session: atuin::session::Session,
    /// Cached NUL-terminated session UUID. Returning a pointer to this field
    /// avoids allocating (and leaking) a new CString on every UUID query.
    uuid: CString,
}

/// Run `f` with the global session, or fail when `atuin_init` has not been called.
///
/// The lock is held for the duration of one native operation. Shell hosts are
/// single-threaded, so this is serialization, not a concurrency feature; the
/// mutex is what makes the global state sound for a multi-threaded host.
fn with_session<T, E: std::fmt::Display>(
    f: impl FnOnce(&mut SessionHandle) -> Result<T, E>,
) -> Result<T, String> {
    let mut guard = SESSION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(session) = guard.as_mut() else {
        return Err("session is not initialized".to_string());
    };
    f(session).map_err(|e| format!("{e:#}"))
}

/// Reject calls made from a forked child process.
///
/// zsh forks for $(...), &, pipelines, subshells, and process substitution. The
/// child inherits the parent's tokio runtime in a corrupted state, so it must
/// not touch or shut down the session. Returning before locking `SESSION` also
/// avoids blocking on a mutex that may have been held during `fork()`.
macro_rules! guard_fork {
    () => {
        let recorded_pid = SESSION_PID.load(Ordering::Relaxed);
        if recorded_pid != 0 && recorded_pid != std::process::id() {
            return error_string("refusing call in forked child process");
        }
    };
}

// ---------------------------------------------------------------------------
// Search options (mirrors the upstream `atuin search` non-interactive flags)
// ---------------------------------------------------------------------------

pub const ATUIN_SEARCH_MODE_AUTO: c_int = 0;
pub const ATUIN_SEARCH_MODE_PREFIX: c_int = 1;
pub const ATUIN_SEARCH_MODE_FULLTEXT: c_int = 2;
pub const ATUIN_SEARCH_MODE_FUZZY: c_int = 3;
pub const ATUIN_SEARCH_MODE_DAEMON_FUZZY: c_int = 4;

pub const ATUIN_FILTER_MODE_AUTO: c_int = 0;
pub const ATUIN_FILTER_MODE_GLOBAL: c_int = 1;
pub const ATUIN_FILTER_MODE_HOST: c_int = 2;
pub const ATUIN_FILTER_MODE_SESSION: c_int = 3;
pub const ATUIN_FILTER_MODE_DIRECTORY: c_int = 4;
pub const ATUIN_FILTER_MODE_WORKSPACE: c_int = 5;
pub const ATUIN_FILTER_MODE_SESSION_PRELOAD: c_int = 6;

/// Interactive TUI keymap mode (mirrors `atuin search --keymap-mode`).
pub const ATUIN_KEYMAP_MODE_AUTO: c_int = 0;
pub const ATUIN_KEYMAP_MODE_EMACS: c_int = 1;
pub const ATUIN_KEYMAP_MODE_VIM_NORMAL: c_int = 2;
pub const ATUIN_KEYMAP_MODE_VIM_INSERT: c_int = 3;

fn keymap_mode(value: c_int) -> KeymapMode {
    match value {
        ATUIN_KEYMAP_MODE_EMACS => KeymapMode::Emacs,
        ATUIN_KEYMAP_MODE_VIM_NORMAL => KeymapMode::VimNormal,
        ATUIN_KEYMAP_MODE_VIM_INSERT => KeymapMode::VimInsert,
        _ => KeymapMode::Auto,
    }
}

/// C-compatible snapshot of the session's operation counters.
///
/// Mirrors the starship in-process session stats: the shell can show what the
/// native session has done without spawning a child process.
#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct atuin_stats {
    /// `history_start` calls that reached the session.
    pub history_starts: u64,
    /// Blocking `history_end` calls.
    pub history_ends_sync: u64,
    /// Fire-and-forget `history_end` calls.
    pub history_ends_async: u64,
    /// Every search dispatch (interactive and non-interactive).
    pub search_calls: u64,
    /// `search_prefix` shortcut calls (also counted in `search_calls`).
    pub search_prefix_calls: u64,
    /// Interactive TUI sessions opened.
    pub interactive_search_calls: u64,
    /// Interactive searches that accepted a history entry.
    pub interactive_selections: u64,
    /// Interactive searches the user cancelled.
    pub interactive_cancels: u64,
    /// Fire-and-forget `history_end` writes still pending.
    pub in_flight_history_ends: u64,
    /// Seconds since the session was created.
    pub uptime_secs: u64,
}

/// C-compatible search options. `query` and the string fields are borrowed for
/// the duration of the call. `authors`/`shells` are arrays of
/// `author_count`/`shell_count` pointers, each NUL-terminated.
/// `exits`/`exclude_exits` are arrays of `exit_count`/`exclude_exit_count`
/// 64-bit exit codes (mirrors the upstream repeatable `--exit` /
/// `--exclude-exit`); an empty array means no restriction.
#[repr(C)]
pub struct atuin_search_options {
    pub query: *const c_char,
    pub search_mode: c_int,
    pub filter_mode: c_int,
    pub cwd: *const c_char,
    pub exclude_cwd: *const c_char,
    pub exits: *const i64,
    pub exit_count: usize,
    pub exclude_exits: *const i64,
    pub exclude_exit_count: usize,
    pub before: *const c_char,
    pub after: *const c_char,
    pub has_limit: c_int,
    pub limit: i64,
    pub has_offset: c_int,
    pub offset: i64,
    pub reverse: c_int,
    pub include_duplicates: c_int,
    pub authors: *const *const c_char,
    pub author_count: usize,
    pub shells: *const *const c_char,
    pub shell_count: usize,
}

fn cstr_opt(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: callers pass valid NUL-terminated strings for the duration of
    // the call.
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .ok()
        .map(str::to_string)
}

unsafe fn cstr_array_to_vec(ptr: *const *const c_char, count: usize) -> Option<Vec<String>> {
    if count == 0 {
        return Some(Vec::new());
    }
    if ptr.is_null() {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let item = unsafe { *ptr.add(i) };
        if item.is_null() {
            return None;
        }
        out.push(unsafe { CStr::from_ptr(item) }.to_str().ok()?.to_owned());
    }
    Some(out)
}

unsafe fn i64_array_to_vec(ptr: *const i64, count: usize) -> Option<Vec<i64>> {
    if count == 0 {
        return Some(Vec::new());
    }
    if ptr.is_null() {
        return None;
    }
    // SAFETY: callers guarantee the array holds `count` readable `i64` values
    // for the duration of the call.
    Some(unsafe { std::slice::from_raw_parts(ptr, count) }.to_vec())
}

/// Parse the `--author-kind` value (`user` / `agent`, case-insensitive).
fn parse_author_kind(value: *const c_char) -> Result<Option<AuthorKind>, String> {
    let Some(text) = cstr_opt(value) else {
        return Ok(None);
    };
    match text.to_ascii_lowercase().as_str() {
        "user" => Ok(Some(AuthorKind::User)),
        "agent" => Ok(Some(AuthorKind::Agent)),
        _ => Err(format!("invalid author_kind: {text}")),
    }
}

fn requested_search_mode(value: c_int) -> Option<RequestedSearchMode> {
    match value {
        ATUIN_SEARCH_MODE_PREFIX => Some(RequestedSearchMode::Prefix),
        ATUIN_SEARCH_MODE_FULLTEXT => Some(RequestedSearchMode::Fulltext),
        ATUIN_SEARCH_MODE_FUZZY => Some(RequestedSearchMode::Fuzzy),
        ATUIN_SEARCH_MODE_DAEMON_FUZZY => Some(RequestedSearchMode::DaemonFuzzy),
        _ => None,
    }
}

fn filter_mode(value: c_int) -> Option<FilterMode> {
    match value {
        ATUIN_FILTER_MODE_GLOBAL => Some(FilterMode::Global),
        ATUIN_FILTER_MODE_HOST => Some(FilterMode::Host),
        ATUIN_FILTER_MODE_SESSION => Some(FilterMode::Session),
        ATUIN_FILTER_MODE_DIRECTORY => Some(FilterMode::Directory),
        ATUIN_FILTER_MODE_WORKSPACE => Some(FilterMode::Workspace),
        ATUIN_FILTER_MODE_SESSION_PRELOAD => Some(FilterMode::SessionPreload),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Public C API
// ---------------------------------------------------------------------------

/// Create the process-wide history session.
///
/// The Atuin data directory is resolved from the user's configuration exactly
/// like the official CLI: `ATUIN_DATA_DIR` / XDG, or `data_dir` in config.toml.
///
/// Returns NULL on success. Calling it while a session is already active is a
/// successful no-op that keeps the existing session.
#[unsafe(no_mangle)]
pub extern "C" fn atuin_init() -> *mut c_char {
    atuin_init_with_datadir(ptr::null())
}

/// Create the session for an explicit data directory.
///
/// Deliberately **not** an exported symbol: shell integrations must only use
/// [`atuin_init`] so they always follow the user's configuration.
/// The unit tests call this directly to give every test its own data directory.
/// `data_dir == NULL` follows the fully-resolved settings, exactly like
/// [`atuin_init`].
///
/// # Safety
/// `data_dir`, when non-null, must point to a valid NUL-terminated C string for
/// the duration of this call.
fn atuin_init_with_datadir(data_dir: *const c_char) -> *mut c_char {
    ffi_guard_error!({
        guard_fork!();
        let dir: Option<PathBuf> = if data_dir.is_null() {
            None
        } else {
            // SAFETY: data_dir was checked non-null; callers guarantee it is a
            // valid NUL-terminated UTF-8 C string for the duration of this call.
            match unsafe { CStr::from_ptr(data_dir) }.to_str() {
                Ok(s) => Some(PathBuf::from(s)),
                Err(_) => return error_string("data_dir is not valid UTF-8"),
            }
        };

        let mut guard = SESSION
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if guard.is_none() {
            let session = match atuin::session::Session::new_with_datadir(dir.as_deref()) {
                Ok(s) => s,
                Err(e) => return error_string(format!("{e:#}")),
            };

            let uuid = match CString::new(session.session_id_str().to_string()) {
                Ok(uuid) => uuid,
                Err(_) => {
                    return error_string("session UUID contains a NUL byte");
                }
            };

            *guard = Some(SessionHandle { session, uuid });
            SESSION_PID.store(std::process::id(), Ordering::Relaxed);
        }

        ptr::null_mut()
    })
}

/// Destroy the process-wide session.
///
/// Passing no session (never created, or already destroyed) is a successful
/// no-op, so the call is idempotent. The Session's `Drop` implementation shuts
/// down the tokio runtime and the SQLite pools before the library can be
/// unloaded.
///
/// Returns NULL on success, or an allocated error string (free with
/// [`atuin_free`]).
#[unsafe(no_mangle)]
pub extern "C" fn atuin_shutdown() -> *mut c_char {
    ffi_guard_error!({
        guard_fork!();

        // Drop the session while holding the lock so a concurrent init cannot
        // start a new session before the tokio runtime, SQLite pools and meta
        // store have finished shutting down.
        let mut guard = SESSION
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        SESSION_PID.store(0, Ordering::Relaxed);
        *guard = None;
        ptr::null_mut()
    })
}

/// Record a command start.
///
/// On success returns NULL and writes a Rust-allocated UTF-8 history ID to
/// `*id_out`; the caller must free it with [`atuin_free`]. `*id_out` may stay
/// NULL when Atuin's exclusion filters drop the command. On failure returns an
/// allocated error string and resets `*id_out` to NULL.
///
/// `author` / `author_kind` / `intent` mirror the official
/// `atuin history start --author / --author-kind / --intent` flags.
/// `author_kind` accepts `user` or `agent` (case-insensitive); NULL performs the
/// same best-effort probe as the CLI (`ATUIN_HISTORY_AUTHOR_KIND`).
///
/// # Safety
/// `command` must be a valid NUL-terminated C string. `cwd`, `author`,
/// `author_kind` and `intent` must be NULL or valid NUL-terminated C strings.
/// `id_out` must be NULL or point to a writable `char *` slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_history_start(
    command: *const c_char,
    cwd: *const c_char,
    author: *const c_char,
    author_kind: *const c_char,
    intent: *const c_char,
    id_out: *mut *mut c_char,
) -> *mut c_char {
    ffi_guard_error!({
        if id_out.is_null() {
            return error_string("null argument");
        }

        // Always reset the caller's output slot before any other validation:
        // on failure the caller may otherwise keep a stale pointer from a
        // previous successful call.
        unsafe { *id_out = ptr::null_mut() };

        if command.is_null() {
            return error_string("null argument");
        }
        guard_fork!();

        // Invalid UTF-8 or an embedded NUL cannot be represented in a C string.
        // `to_str` failure is treated as an empty string, matching how `atuin`
        // handles unrepresentable command data.
        let cmd = unsafe { CStr::from_ptr(command) }.to_str().unwrap_or("");
        let cwd_str = if cwd.is_null() {
            ""
        } else {
            unsafe { CStr::from_ptr(cwd) }.to_str().unwrap_or("")
        };
        let auth = if author.is_null() {
            None
        } else {
            unsafe { CStr::from_ptr(author) }.to_str().ok()
        };
        let kind = match parse_author_kind(author_kind) {
            Ok(kind) => kind,
            Err(e) => return error_string(e),
        };
        let int = if intent.is_null() {
            None
        } else {
            unsafe { CStr::from_ptr(intent) }.to_str().ok()
        };

        let id = match with_session(|session| {
            session.session.history_start(cmd, cwd_str, auth, kind, int)
        }) {
            Ok(id) => id,
            Err(e) => return error_string(format!("{e:#}")),
        };

        if let Some(id) = id {
            let id_ptr = match CString::new(id) {
                Ok(id) => id,
                Err(_) => {
                    return error_string("history ID contains a NUL byte");
                }
            };
            unsafe {
                *id_out = id_ptr.into_raw();
            }
        }

        ptr::null_mut()
    })
}

/// Finalise a command started with [`atuin_history_start`].
///
/// When `sync` is non-zero, blocks until the database update completes and
/// returns NULL on success / an allocated error string on failure. When `sync`
/// is 0, spawns the work on a tokio multi-thread worker and returns
/// immediately — matching the official `(atuin history end ... &)`
/// fire-and-forget behavior. Async errors are logged by the Atuin session and
/// are not reflected in the return value.
///
/// `duration_ns` is signed to match the C `long long` declaration and must be
/// non-negative; a negative value is rejected with an error instead of being
/// reinterpreted as a huge unsigned duration. 0 means "infer from the start
/// timestamp", exactly like the official CLI when `--duration` is omitted.
///
/// # Safety
/// `id` must be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_history_end(
    id: *const c_char,
    exit_code: i64,
    duration_ns: i64,
    sync: c_int,
) -> *mut c_char {
    ffi_guard_error!({
        if id.is_null() {
            return error_string("null argument");
        }
        guard_fork!();

        if duration_ns < 0 {
            return error_string("duration_ns must not be negative");
        }
        let duration_ns = duration_ns as u64;

        let id_str = unsafe { CStr::from_ptr(id) }
            .to_str()
            .unwrap_or("")
            .to_string();

        match with_session(|session| {
            if sync != 0 {
                return session.session.history_end(&id_str, exit_code, duration_ns);
            } else {
                session
                    .session
                    .history_end_async(id_str, exit_code, duration_ns);
                Ok(())
            }
        }) {
            Ok(_) => return ptr::null_mut(),
            Err(e) => return error_string(format!("{e:#}")),
        };
    })
}

/// Search history with upstream-compatible non-interactive options.
///
/// On success returns NULL and writes newline-separated command texts to `*out`
/// (the caller must free it with [`atuin_free`]). An empty result is a non-null
/// empty string.
///
/// # Safety
/// `options` must be NULL or a valid `atuin_search_options` for the duration of
/// the call. `out` must be NULL or point to a writable `char *` slot. The
/// `authors`/`shells` arrays must contain `author_count`/`shell_count` valid
/// NUL-terminated C strings, and `exits`/`exclude_exits` must contain
/// `exit_count`/`exclude_exit_count` readable `i64` values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_search(
    options: *const atuin_search_options,
    out: *mut *mut c_char,
) -> *mut c_char {
    ffi_guard_error!({
        if out.is_null() {
            return error_string("null argument");
        }

        unsafe { *out = ptr::null_mut() };

        if options.is_null() {
            return error_string("null argument");
        }
        guard_fork!();

        // SAFETY: caller guarantees `options` is valid for the call.
        let o = unsafe { &*options };

        let Some(authors) = (unsafe { cstr_array_to_vec(o.authors, o.author_count) }) else {
            return error_string("invalid authors array");
        };
        let Some(shells) = (unsafe { cstr_array_to_vec(o.shells, o.shell_count) }) else {
            return error_string("invalid shells array");
        };
        let Some(exit) = (unsafe { i64_array_to_vec(o.exits, o.exit_count) }) else {
            return error_string("invalid exits array");
        };
        let Some(exclude_exit) =
            (unsafe { i64_array_to_vec(o.exclude_exits, o.exclude_exit_count) })
        else {
            return error_string("invalid exclude_exits array");
        };

        let search_options = SearchOptions {
            query: cstr_opt(o.query).unwrap_or_default(),
            search_mode: requested_search_mode(o.search_mode),
            filter_mode: filter_mode(o.filter_mode),
            cwd: cstr_opt(o.cwd),
            exclude_cwd: cstr_opt(o.exclude_cwd),
            // Mirrors the upstream repeatable `--exit` / `--exclude-exit`: an
            // empty array means no restriction.
            exit,
            exclude_exit,
            before: cstr_opt(o.before),
            after: cstr_opt(o.after),
            limit: (o.has_limit != 0).then_some(o.limit),
            offset: (o.has_offset != 0).then_some(o.offset),
            reverse: o.reverse != 0,
            include_duplicates: o.include_duplicates != 0,
            authors: authors.into_iter().map(AuthorPattern::from).collect(),
            shells,
            mode: SearchMode::NonInteractive,
            ..Default::default()
        };

        let results = match with_session(|session| session.session.search(search_options)) {
            Ok(SearchResult::Entries(entries)) => entries,
            Ok(SearchResult::Interactive(_)) => {
                return error_string("interactive mode is not available here");
            }
            Err(e) => return error_string(format!("{e:#}")),
        };

        let output = results
            .iter()
            .map(|item| item.command.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        unsafe {
            *out = string_into_c(output);
        }
        ptr::null_mut()
    })
}

/// Search history by prefix. On success returns NULL and writes
/// newline-separated commands to `*out` (caller must free with
/// [`atuin_free`]).
///
/// `query` may be NULL (treated as the empty string). `limit <= 0` returns an
/// empty (but non-null on success) result.
///
/// # Safety
/// `query` must be NULL or a valid NUL-terminated C string. `out` must be NULL
/// or point to a writable `char *` slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_search_prefix(
    query: *const c_char,
    limit: c_int,
    out: *mut *mut c_char,
) -> *mut c_char {
    ffi_guard_error!({
        if out.is_null() {
            return error_string("null argument");
        }

        unsafe { *out = ptr::null_mut() };
        guard_fork!();

        let q = if query.is_null() {
            ""
        } else {
            unsafe { CStr::from_ptr(query) }.to_str().unwrap_or("")
        };

        let limit = if limit < 0 { 0 } else { limit as usize };

        let results = match with_session(|session| session.session.search_prefix(q, limit)) {
            Ok(r) => r,
            Err(e) => return error_string(format!("{e:#}")),
        };

        let output = results
            .iter()
            .map(|h| h.command.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        unsafe {
            *out = string_into_c(output);
        }
        ptr::null_mut()
    })
}

/// Interactive TUI search over the in-process session's history.
///
/// Opens a full-screen ratatui UI on the controlling terminal, prefilled with
/// `query`. This is the native replacement for `atuin search --interactive`
/// and requires a controlling terminal (stdout may be redirected; output and
/// input then use `/dev/tty` / `CONOUT$`).
///
/// `shell_up_key_binding` and `keymap_mode` mirror the official
/// `--shell-up-key-binding` / `--keymap-mode` flags. They select the matching
/// session quick path: `search_interactive_up` when only the UpArrow flag is
/// set, `search_interactive_with` when an explicit keymap mode is given, and
/// plain `search_interactive` otherwise.
///
/// On success (including when the user cancels) returns NULL. `*out` receives
/// a Rust-allocated UTF-8 string when a command was selected (free with
/// [`atuin_free`]), or stays NULL when the user cancelled and the shell should
/// leave its buffer unchanged. The selected string is prefixed with
/// `__atuin_accept__:` when the shell should execute it immediately (per
/// `enter_accept` config).
///
/// This call blocks the shell's main thread while the TUI is open, exactly
/// like launching the official interactive-search process. Raw mode and the
/// alternate screen are restored before returning, including on panic.
///
/// # Safety
/// `query` must be NULL or a valid NUL-terminated C string. `out` must be NULL
/// or point to a writable `char *` slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_search_interactive(
    query: *const c_char,
    shell_up_key_binding: c_int,
    keymap_mode_value: c_int,
    out: *mut *mut c_char,
) -> *mut c_char {
    ffi_guard_error!({
        if out.is_null() {
            return error_string("null argument");
        }

        unsafe { *out = ptr::null_mut() };
        guard_fork!();

        let q = if query.is_null() {
            ""
        } else {
            unsafe { CStr::from_ptr(query) }.to_str().unwrap_or("")
        };

        let up = shell_up_key_binding != 0;

        match with_session(|session| {
            if keymap_mode_value != ATUIN_KEYMAP_MODE_AUTO {
                session.session.search_interactive_with(
                    SearchOptions::interactive(q)
                        .with_shell_up_key_binding(up)
                        .with_keymap_mode(keymap_mode(keymap_mode_value)),
                )
            } else if up {
                session.session.search_interactive_up(q)
            } else {
                session.session.search_interactive(q)
            }
        }) {
            Ok(Some(selected)) => {
                unsafe {
                    *out = string_into_c(selected);
                }
                ptr::null_mut()
            }
            // Cancel is a successful call with no selected value.
            Ok(None) => ptr::null_mut(),
            Err(e) => error_string(format!("{e:#}")),
        }
    })
}

/// Snapshot the active session's operation counters.
///
/// On success returns NULL and writes the snapshot to `*out`. When there is no
/// active session it returns an allocated error string (free with
/// [`atuin_free`]) and leaves `*out` zeroed.
///
/// # Safety
/// `out` must be NULL or point to writable `atuin_stats` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_stats(out: *mut atuin_stats) -> *mut c_char {
    ffi_guard_error!({
        if out.is_null() {
            return error_string("null argument");
        }
        // Reset before any other validation so a failed call never leaves the
        // caller with a stale snapshot.
        unsafe { *out = atuin_stats::default() };
        guard_fork!();

        let stats =
            match with_session::<SessionStats, String>(|session| Ok(session.session.stats())) {
                Ok(stats) => stats,
                Err(e) => return error_string(e),
            };

        let c_stats = atuin_stats {
            history_starts: stats.history_starts,
            history_ends_sync: stats.history_ends_sync,
            history_ends_async: stats.history_ends_async,
            search_calls: stats.search_calls,
            search_prefix_calls: stats.search_prefix_calls,
            interactive_search_calls: stats.interactive_search_calls,
            interactive_selections: stats.interactive_selections,
            interactive_cancels: stats.interactive_cancels,
            in_flight_history_ends: stats.in_flight_history_ends,
            uptime_secs: stats.uptime_secs,
        };
        // SAFETY: `out` is writable for the duration of this call.
        unsafe { *out = c_stats };
        ptr::null_mut()
    })
}

/// Free a string previously returned by a fallible `atuin_*` function or by
/// [`atuin_history_start`] / [`atuin_search`] / [`atuin_search_prefix`] /
/// [`atuin_search_interactive`].
///
/// Passing NULL is safe (no-op). This function cannot fail, so it is the one
/// export that does not use the `char *` error protocol.
///
/// # Safety
/// `ptr` must be NULL or a pointer previously returned by this library exactly
/// once and not freed before. Static strings from [`atuin_version`] and the
/// session-owned UUID from [`atuin_session_uuid`] must NOT be passed here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_free(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: contract requires ptr to have been returned by this library
    // exactly once and not freed before.
    unsafe {
        let _ = CString::from_raw(ptr);
    }
}

/// Return the session UUID.
///
/// On success returns NULL and writes a session-owned pointer to `*out`; the
/// pointer stays valid until the session is destroyed and must NOT be freed.
/// On failure returns an allocated error string (free with [`atuin_free`]) and
/// sets `*out` to NULL.
///
/// # Safety
/// `out` must be NULL or point to a writable `*const c_char` slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_session_uuid(out: *mut *const c_char) -> *mut c_char {
    ffi_guard_error!({
        if out.is_null() {
            return error_string("null argument");
        }

        unsafe { *out = ptr::null() };
        guard_fork!();

        let uuid_ptr = match with_session::<*const i8, String>(|session| Ok(session.uuid.as_ptr()))
        {
            Ok(uuid_ptr) => uuid_ptr,
            Err(e) => return error_string(e),
        };

        unsafe {
            *out = uuid_ptr;
        }
        ptr::null_mut()
    })
}

/// Return the library version as a static string.
///
/// The returned pointer is valid for the lifetime of the process and must NOT
/// be freed. This accessor cannot fail, so it is exempt from the error
/// protocol.
#[unsafe(no_mangle)]
pub extern "C" fn atuin_version() -> *const c_char {
    // A string literal has static storage duration and the trailing NUL is
    // included in the literal itself, so no LazyLock/allocation is needed.
    static VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
    VERSION.as_ptr().cast()
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::ptr;
    use std::sync::MutexGuard;

    /// Serializes session tests.
    ///
    /// The library exposes a single process-global session, so tests cannot
    /// hold sessions at the same time. Each [`TestSession`] takes this lock for
    /// its whole lifetime and still gets its own temporary data directory.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// RAII test session: holds the global session lock, a temporary data
    /// directory, and destroys the native session on drop.
    struct TestSession {
        // Declared first so it is released after `Drop::drop` destroyed the
        // session (Rust drops `Drop::drop` first, then the fields in order).
        _lock: MutexGuard<'static, ()>,
        tmp: tempfile::TempDir,
    }

    impl TestSession {
        fn new() -> Self {
            let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let tmp = tempfile::tempdir().unwrap();
            let path = CString::new(tmp.path().to_str().unwrap()).unwrap();
            let err = atuin_init_with_datadir(path.as_ptr());
            if !err.is_null() {
                panic!("session creation failed: {}", take_error(err));
            }
            Self { _lock: lock, tmp }
        }

        fn path(&self) -> &std::path::Path {
            self.tmp.path()
        }

        /// Read `(exit, duration)` for `id` from the active session.
        fn history_state(&self, id: &str) -> Option<(i64, i64)> {
            match with_session::<Option<(i64, i64)>, String>(|session| {
                Ok(session
                    .session
                    .get_history(id)
                    .unwrap()
                    .map(|h| (h.exit, h.duration)))
            }) {
                Ok(state) => state,
                Err(e) => panic!("{e:#}"),
            }
        }
    }

    impl Drop for TestSession {
        fn drop(&mut self) {
            let err = atuin_shutdown();
            assert_ok(err, "session_destroy");
        }
    }

    /// Read and free an allocated C error string.
    fn take_error(err: *mut c_char) -> String {
        assert!(!err.is_null(), "expected an error string");
        let msg = unsafe { CStr::from_ptr(err) }
            .to_string_lossy()
            .into_owned();
        unsafe { atuin_free(err) };
        msg
    }

    /// Panic with the returned error string when `err` is non-NULL.
    fn assert_ok(err: *mut c_char, operation: &str) {
        if !err.is_null() {
            panic!("{operation} failed: {}", take_error(err));
        }
    }

    /// Read and free a successful out string.
    fn take_string(ptr: *mut c_char) -> String {
        assert!(!ptr.is_null(), "expected a non-null string");
        let text = unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        unsafe { atuin_free(ptr) };
        text
    }

    /// Read the current session UUID into an owned string.
    fn session_uuid() -> String {
        let mut out: *const c_char = ptr::null();
        let err = unsafe { atuin_session_uuid(&mut out) };
        assert_ok(err, "session_uuid");
        assert!(!out.is_null(), "expected a non-null UUID");
        unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string()
    }

    /// Start a command in the active session, returning the caller-owned ID.
    fn start_test_command(command: &str) -> String {
        let cmd = CString::new(command).unwrap();
        let cwd = CString::new("/tmp").unwrap();
        let mut id_ptr: *mut c_char = ptr::null_mut();
        let err = unsafe {
            atuin_history_start(
                cmd.as_ptr(),
                cwd.as_ptr(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                &mut id_ptr,
            )
        };
        assert_ok(err, "history_start");
        take_string(id_ptr)
    }

    fn search_prefix_text(query: *const c_char, limit: c_int) -> String {
        let mut out: *mut c_char = ptr::null_mut();
        let err = unsafe { atuin_search_prefix(query, limit, &mut out) };
        assert_ok(err, "search_prefix");
        take_string(out)
    }

    /// Start a command with explicit author fields, returning the history ID.
    fn start_command_full(
        command: &str,
        author: Option<&str>,
        author_kind: Option<&str>,
        intent: Option<&str>,
    ) -> String {
        let cmd = CString::new(command).unwrap();
        let cwd = CString::new("/tmp").unwrap();
        let author = author.map(|a| CString::new(a).unwrap());
        let kind = author_kind.map(|k| CString::new(k).unwrap());
        let intent = intent.map(|i| CString::new(i).unwrap());
        let mut id_ptr: *mut c_char = ptr::null_mut();
        let err = unsafe {
            atuin_history_start(
                cmd.as_ptr(),
                cwd.as_ptr(),
                author.as_ref().map_or(ptr::null(), |c| c.as_ptr()),
                kind.as_ref().map_or(ptr::null(), |c| c.as_ptr()),
                intent.as_ref().map_or(ptr::null(), |c| c.as_ptr()),
                &mut id_ptr,
            )
        };
        assert_ok(err, "history_start");
        take_string(id_ptr)
    }

    /// Finalise `id` synchronously with `exit`.
    fn end_command(id: &str, exit: i64) {
        let id_c = CString::new(id).unwrap();
        let err = unsafe { atuin_history_end(id_c.as_ptr(), exit, 0, 1) };
        assert_ok(err, "history_end");
    }

    /// Build non-interactive search options with the repeatable filters.
    fn make_search_options(
        query: *const c_char,
        exits: &[i64],
        exclude_exits: &[i64],
        authors: &[*const c_char],
    ) -> atuin_search_options {
        atuin_search_options {
            query,
            search_mode: ATUIN_SEARCH_MODE_PREFIX,
            filter_mode: ATUIN_FILTER_MODE_AUTO,
            cwd: ptr::null(),
            exclude_cwd: ptr::null(),
            exits: exits.as_ptr(),
            exit_count: exits.len(),
            exclude_exits: exclude_exits.as_ptr(),
            exclude_exit_count: exclude_exits.len(),
            before: ptr::null(),
            after: ptr::null(),
            has_limit: 0,
            limit: 0,
            has_offset: 0,
            offset: 0,
            reverse: 0,
            include_duplicates: 0,
            authors: authors.as_ptr(),
            author_count: authors.len(),
            shells: ptr::null(),
            shell_count: 0,
        }
    }

    fn run_search(options: &atuin_search_options) -> String {
        let mut out: *mut c_char = ptr::null_mut();
        let err = unsafe { atuin_search(options, &mut out) };
        assert_ok(err, "atuin_search");
        take_string(out)
    }

    #[test]
    fn test_create_and_destroy() {
        // `TestSession::drop` asserts that destroy succeeds.
        let _session = TestSession::new();
    }

    #[test]
    fn test_destroy_without_session_is_safe() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let err = atuin_shutdown();
        assert!(err.is_null(), "destroying without a session is a no-op");
    }

    #[test]
    fn test_create_twice_does_not_fail() {
        let _session = TestSession::new();
        let err = atuin_init();
        assert!(err.is_null(), "unexpected error: {}", take_error(err));
    }

    #[test]
    fn test_calls_without_session_fail() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let cmd = CString::new("echo no-session").unwrap();
        let cwd = CString::new("/tmp").unwrap();
        let mut id: *mut c_char = ptr::null_mut();
        let err = unsafe {
            atuin_history_start(
                cmd.as_ptr(),
                cwd.as_ptr(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                &mut id,
            )
        };
        let msg = take_error(err);
        assert!(msg.contains("session is not initialized"), "unexpected error: {msg}");
        assert!(id.is_null());

        let mut out: *mut c_char = ptr::null_mut();
        let err = unsafe { atuin_search_prefix(ptr::null(), 1, &mut out) };
        let msg = take_error(err);
        assert!(msg.contains("session is not initialized"), "unexpected error: {msg}");
        assert!(out.is_null());

        let mut uuid: *const c_char = ptr::null();
        let err = unsafe { atuin_session_uuid(&mut uuid) };
        let msg = take_error(err);
        assert!(msg.contains("session is not initialized"), "unexpected error: {msg}");
        assert!(uuid.is_null());
    }

    #[test]
    fn test_recreate_after_destroy() {
        let first = {
            let _session = TestSession::new();
            session_uuid()
        };
        let second = {
            let _session = TestSession::new();
            session_uuid()
        };
        assert_ne!(first, second, "a recreated session must have a new UUID");
    }

    #[test]
    fn test_free_null_is_safe() {
        unsafe {
            atuin_free(ptr::null_mut());
        }
    }

    #[test]
    fn test_version_is_static_nonempty_string() {
        let v = atuin_version();
        assert!(!v.is_null());
        let s = unsafe { CStr::from_ptr(v) }.to_str().unwrap();
        assert_eq!(s, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn test_history_start_rejects_null_arguments() {
        let _session = TestSession::new();

        let mut id_ptr: *mut c_char = ptr::null_mut();
        let err = unsafe {
            atuin_history_start(
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                &mut id_ptr,
            )
        };
        assert!(!err.is_null(), "null command should return an error");
        assert!(id_ptr.is_null(), "failed call must reset output pointer");
        take_error(err);

        let err = unsafe {
            atuin_history_start(
                CString::new("echo").unwrap().as_ptr(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
            )
        };
        assert!(!err.is_null(), "null id_out should return an error");
        take_error(err);
    }

    #[test]
    fn test_history_end_rejects_null_arguments() {
        let _session = TestSession::new();

        let err = unsafe { atuin_history_end(ptr::null(), 0, 0, 1) };
        assert!(!err.is_null());
        take_error(err);
    }

    /// A negative duration must be rejected rather than wrapping to a huge
    /// unsigned value; the C declaration is signed `long long`.
    #[test]
    fn test_history_end_rejects_negative_duration() {
        let _session = TestSession::new();
        let id = start_test_command("echo negative-duration");
        let id_c = CString::new(id.as_str()).unwrap();

        for sync in [0, 1] {
            let err = unsafe { atuin_history_end(id_c.as_ptr(), 0, -1, sync) };
            let msg = take_error(err);
            assert!(
                msg.contains("duration_ns must not be negative"),
                "unexpected error: {msg}"
            );
        }
    }

    #[test]
    fn test_history_roundtrip_sync() {
        let _session = TestSession::new();
        let id = start_test_command("echo hello from ffi");

        let id_c = CString::new(id).unwrap();
        let err = unsafe { atuin_history_end(id_c.as_ptr(), 0, 1_000_000, 1) };
        assert_ok(err, "history_end");

        let query = CString::new("echo hello").unwrap();
        let results = search_prefix_text(query.as_ptr(), 10);
        assert!(results.contains("echo hello from ffi"), "got: {results}");
    }

    #[test]
    fn test_history_end_async_completes_on_worker_thread() {
        let session = TestSession::new();
        let id = start_test_command("echo worker-thread-async");
        let id_c = CString::new(id.as_str()).unwrap();

        // sync=0 is the precmd fast path: it must return before the DB update
        // has necessarily finished, and the multi-thread runtime must complete
        // the update on a worker thread without another FFI call to drive it.
        let err = unsafe { atuin_history_end(id_c.as_ptr(), 7, 123_456, 0) };
        assert_ok(err, "history_end async");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if session.history_state(&id) == Some((7, 123_456)) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fire-and-forget history_end did not complete on a worker thread"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn test_history_end_persists_record_store() {
        let session = TestSession::new();
        let id = start_test_command("echo record-store-test");
        let id_c = CString::new(id.as_str()).unwrap();
        let err = unsafe { atuin_history_end(id_c.as_ptr(), 0, 42, 1) };
        assert_ok(err, "history_end");

        let records = session.path().join("records.db");
        assert!(
            records.exists(),
            "history_end should append to the record store: {}",
            records.display()
        );
        let meta = std::fs::metadata(&records).unwrap();
        assert!(
            meta.len() > 0,
            "records.db should not be empty after history_end"
        );
    }

    #[test]
    fn test_history_end_async_does_not_crash() {
        let _session = TestSession::new();
        let id = start_test_command("echo async-atuin-test");
        let id_c = CString::new(id.as_str()).unwrap();
        let err = unsafe { atuin_history_end(id_c.as_ptr(), 0, 0, 0) };
        assert_ok(err, "history_end async");

        // Calling sync end with the same ID immediately afterwards must also be
        // safe: whichever update wins sets duration, and the other skips.
        let id_c = CString::new(id).unwrap();
        let err = unsafe { atuin_history_end(id_c.as_ptr(), 0, 0, 1) };
        assert_ok(err, "history_end sync");
    }

    #[test]
    fn test_search_limits_results() {
        let _session = TestSession::new();

        let first = start_test_command("alpha first command");
        let second = start_test_command("alpha second command");
        assert_ne!(first, second);

        let query = CString::new("alpha").unwrap();
        let results = search_prefix_text(query.as_ptr(), 1);
        assert_eq!(
            results.lines().count(),
            1,
            "limit=1 should return one line: {results}"
        );

        let results = search_prefix_text(query.as_ptr(), 0);
        assert!(results.is_empty(), "limit=0 should return an empty string");
    }

    #[test]
    fn test_search_rejects_null_arguments() {
        let _session = TestSession::new();

        let err = unsafe { atuin_search_prefix(ptr::null(), 1, ptr::null_mut()) };
        assert!(!err.is_null(), "null out should return an error");
        take_error(err);

        let err = unsafe { atuin_search(ptr::null(), ptr::null_mut()) };
        assert!(!err.is_null(), "null out should return an error");
        take_error(err);
    }

    #[test]
    fn test_search_interactive_rejects_null_out() {
        // Argument validation happens before any terminal I/O, so this test is
        // safe even under a TTY: with a valid out pointer the function would
        // open the full-screen UI and block.
        let _session = TestSession::new();
        let err = unsafe {
            atuin_search_interactive(ptr::null(), 0, ATUIN_KEYMAP_MODE_AUTO, ptr::null_mut())
        };
        assert!(
            !err.is_null(),
            "null out pointer must be rejected before the TUI starts"
        );
        take_error(err);
    }

    #[test]
    fn test_stats_without_session_fails() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut stats = atuin_stats::default();
        let err = unsafe { atuin_stats(&mut stats) };
        let msg = take_error(err);
        assert!(msg.contains("session is not initialized"), "unexpected error: {msg}");
        assert_eq!(
            stats.history_starts, 0,
            "failed call must zero the snapshot"
        );
    }

    #[test]
    fn test_stats_counts_operations() {
        let _session = TestSession::new();

        let mut stats = atuin_stats::default();
        let err = unsafe { atuin_stats(&mut stats) };
        assert_ok(err, "atuin_stats");
        assert_eq!(stats.history_starts, 0, "fresh session starts at zero");

        let id = start_test_command("echo stats-count-test");
        let id_c = CString::new(id).unwrap();
        let err = unsafe { atuin_history_end(id_c.as_ptr(), 0, 0, 1) };
        assert_ok(err, "history_end");

        let query = CString::new("echo stats").unwrap();
        let _ = search_prefix_text(query.as_ptr(), 5);

        let mut stats = atuin_stats::default();
        let err = unsafe { atuin_stats(&mut stats) };
        assert_ok(err, "atuin_stats");
        assert_eq!(stats.history_starts, 1, "one history_start");
        assert_eq!(stats.history_ends_sync, 1, "one sync history_end");
        assert_eq!(stats.search_prefix_calls, 1, "one prefix search");
        assert!(stats.search_calls >= 1, "prefix search dispatches search");
    }

    #[test]
    fn test_stats_reset_after_recreate() {
        {
            let _session = TestSession::new();
            let _ = start_test_command("echo stats-recreate-test");
            let mut stats = atuin_stats::default();
            assert_ok(unsafe { atuin_stats(&mut stats) }, "atuin_stats");
            assert_eq!(stats.history_starts, 1);
        }

        // A re-created session must start from zero counters.
        let _session = TestSession::new();
        let mut stats = atuin_stats::default();
        assert_ok(unsafe { atuin_stats(&mut stats) }, "atuin_stats");
        assert_eq!(stats.history_starts, 0, "recreated session starts at zero");
        assert_eq!(stats.search_calls, 0);
    }

    #[test]
    fn test_search_missing_query_returns_empty() {
        let _session = TestSession::new();
        start_test_command("definitely-not-found-command");

        let query = CString::new("no-such-prefix").unwrap();
        let results = search_prefix_text(query.as_ptr(), 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_session_uuid_is_stable() {
        let _session = TestSession::new();

        let mut first: *const c_char = ptr::null();
        let err = unsafe { atuin_session_uuid(&mut first) };
        assert_ok(err, "session_uuid");
        let mut second: *const c_char = ptr::null();
        let err = unsafe { atuin_session_uuid(&mut second) };
        assert_ok(err, "session_uuid");

        assert!(!first.is_null());
        assert_eq!(first, second, "UUID pointer/value must be stable");
        let uuid = unsafe { CStr::from_ptr(first) }.to_str().unwrap();
        assert!(!uuid.is_empty());
    }

    #[test]
    fn test_session_uuid_rejects_null() {
        let out: *const c_char = ptr::null();
        let err = unsafe { atuin_session_uuid(ptr::null_mut()) };
        assert!(!err.is_null());
        assert!(out.is_null());
        take_error(err);
    }

    #[test]
    fn test_search_with_null_query_matches_empty_prefix() {
        let _session = TestSession::new();
        start_test_command("null-query-prefix-test");

        let results = search_prefix_text(ptr::null(), 20);
        assert!(!results.is_empty());
    }

    #[test]
    fn test_command_trailing_newline_is_trimmed() {
        let _session = TestSession::new();
        let id = start_test_command("echo trimmed-command\n");
        let id_c = CString::new(id).unwrap();
        let err = unsafe { atuin_history_end(id_c.as_ptr(), 0, 0, 1) };
        assert_ok(err, "history_end");

        let query = CString::new("echo trimmed-command").unwrap();
        let results = search_prefix_text(query.as_ptr(), 10);
        assert!(results.contains("echo trimmed-command"), "got: {results}");
    }

    /// A failed call must not poison the active session: the session keeps
    /// working afterwards because every error travels in its own return value.
    #[test]
    fn test_errors_do_not_poison_session() {
        let _session = TestSession::new();

        let mut out: *mut c_char = ptr::null_mut();
        let err = unsafe {
            atuin_history_start(
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                &mut out,
            )
        };
        let msg = take_error(err);
        assert!(msg.contains("null"), "unexpected error: {msg}");
        assert!(out.is_null());

        let id = start_test_command("echo after-error");
        let id_c = CString::new(id).unwrap();
        let err = unsafe { atuin_history_end(id_c.as_ptr(), 0, 0, 1) };
        assert_ok(err, "history_end after error");
    }

    /// The single global session serializes concurrent calls instead of each
    /// thread owning a separate session.
    #[test]
    fn test_concurrent_calls_share_one_session() {
        let _session = TestSession::new();

        std::thread::scope(|scope| {
            for i in 0..4 {
                scope.spawn(move || {
                    let command = format!("echo concurrent-{i}");
                    let id = start_test_command(&command);

                    let id_c = CString::new(id).unwrap();
                    let err = unsafe { atuin_history_end(id_c.as_ptr(), 0, 0, 1) };
                    assert_ok(err, "history_end");

                    let query = CString::new(command.clone()).unwrap();
                    let results = search_prefix_text(query.as_ptr(), 5);
                    assert!(
                        results.contains(&command),
                        "thread {i} lost its own result: {results}"
                    );
                });
            }
        });
    }

    /// `atuin history start --author-kind` is forwarded to the session: a
    /// stated kind is authoritative for the `$all-agent` / `$all-user` filters.
    #[test]
    fn test_history_start_author_kind_is_recorded() {
        let _session = TestSession::new();

        let agent_id = start_command_full(
            "echo author-kind-agent",
            Some("claude"),
            Some("agent"),
            Some("testing"),
        );
        end_command(&agent_id, 0);

        let user_id = start_command_full("echo author-kind-user", Some("claude"), Some("user"), None);
        end_command(&user_id, 0);

        let query = CString::new("echo author-kind").unwrap();
        let all_agent = CString::new("$all-agent").unwrap();
        let all_user = CString::new("$all-user").unwrap();

        let agents = [all_agent.as_ptr()];
        let agent_results = run_search(&make_search_options(
            query.as_ptr(),
            &[],
            &[],
            &agents,
        ));
        assert!(
            agent_results.contains("echo author-kind-agent"),
            "stated agent kind not recorded: {agent_results}"
        );
        assert!(
            !agent_results.contains("echo author-kind-user"),
            "stated user kind treated as agent: {agent_results}"
        );

        let users = [all_user.as_ptr()];
        let user_results =
            run_search(&make_search_options(query.as_ptr(), &[], &[], &users));
        assert!(
            user_results.contains("echo author-kind-user"),
            "stated user kind not recorded: {user_results}"
        );
        assert!(
            !user_results.contains("echo author-kind-agent"),
            "stated agent kind treated as user: {user_results}"
        );
    }

    #[test]
    fn test_history_start_rejects_invalid_author_kind() {
        let _session = TestSession::new();

        let cmd = CString::new("echo bad-author-kind").unwrap();
        let cwd = CString::new("/tmp").unwrap();
        let author = CString::new("claude").unwrap();
        let kind = CString::new("robot").unwrap();
        let mut id: *mut c_char = ptr::null_mut();
        let err = unsafe {
            atuin_history_start(
                cmd.as_ptr(),
                cwd.as_ptr(),
                author.as_ptr(),
                kind.as_ptr(),
                ptr::null(),
                &mut id,
            )
        };
        let msg = take_error(err);
        assert!(msg.contains("invalid author_kind"), "unexpected error: {msg}");
        assert!(id.is_null(), "failed call must reset the output pointer");
    }

    /// The repeatable `--exit` / `--exclude-exit` filters accept several codes.
    #[test]
    fn test_search_exit_filters_are_repeatable() {
        let _session = TestSession::new();

        let ok_id = start_test_command("echo exit-filter-ok");
        end_command(&ok_id, 0);

        let one_id = start_test_command("echo exit-filter-one");
        end_command(&one_id, 1);

        let two_id = start_test_command("echo exit-filter-two");
        end_command(&two_id, 2);

        let query = CString::new("echo exit-filter").unwrap();

        // Include any of [1, 2].
        let include = [1_i64, 2];
        let results = run_search(&make_search_options(query.as_ptr(), &include, &[], &[]));
        assert!(results.contains("echo exit-filter-one"), "got: {results}");
        assert!(results.contains("echo exit-filter-two"), "got: {results}");
        assert!(!results.contains("echo exit-filter-ok"), "got: {results}");

        // Exclude [1]: only 0 and 2 remain.
        let exclude = [1_i64];
        let results = run_search(&make_search_options(query.as_ptr(), &[], &exclude, &[]));
        assert!(results.contains("echo exit-filter-ok"), "got: {results}");
        assert!(results.contains("echo exit-filter-two"), "got: {results}");
        assert!(!results.contains("echo exit-filter-one"), "got: {results}");
    }
}
