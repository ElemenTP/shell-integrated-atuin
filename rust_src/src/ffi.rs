//! C FFI exports for Atuin in-process history recording and search.
//!
//! All public functions use the `atuin_` prefix, the C ABI, and follow the
//! ownership rules below:
//!
//! | Function                                | Returned string        | Free with              |
//! |-----------------------------------------|------------------------|------------------------|
//! | `atuin_history_start` → `*id_out`       | Rust-allocated UTF-8   | `atuin_free_string`    |
//! | `atuin_search_prefix` → `*out`          | Rust-allocated UTF-8   | `atuin_free_string`    |
//! | `atuin_search_interactive` → `*out`     | Rust-allocated UTF-8   | `atuin_free_string`    |
//! | `atuin_session_uuid`                    | Session-owned, static  | must NOT be freed      |
//! | `atuin_version`                         | Process-lifetime       | must NOT be freed      |
//! | `atuin_last_error`                      | Valid until next call  | must NOT be freed      |

use libc::c_char;
use std::ffi::{CStr, CString};
use std::os::raw::c_int;
use std::path::PathBuf;
use std::ptr;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------
//
// Deliberately NOT thread_local!: a thread_local! would register a TLS
// destructor on the HOST thread (zsh/pwsh main thread) the first time it is
// touched. After dlclose() unmaps this dylib, that destructor pointer dangles —
// glibc skips destructors of unloaded DSOs, but macOS and Windows do not,
// causing a potential SIGSEGV at host-thread exit.
//
// A global Mutex has no per-thread state and is safe to unload. FFI calls are
// serialized by the shell's single thread anyway, so contention is nil.

static LAST_ERROR: Mutex<Option<CString>> = Mutex::new(None);

fn set_error(msg: &str) {
    if let Ok(mut e) = LAST_ERROR.lock() {
        *e = CString::new(msg).ok();
    }
}

fn clear_error() {
    if let Ok(mut e) = LAST_ERROR.lock() {
        *e = None;
    }
}

/// FFI panic guard: catches Rust panics before they can unwind across the C
/// ABI boundary (undefined behavior) and converts them to an error return.
macro_rules! ffi_guard {
    ($expr:expr, $error_val:expr) => {{
        clear_error();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $expr)) {
            Ok(result) => result,
            Err(panic) => {
                let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                    format!("panic: {s}")
                } else if let Some(s) = panic.downcast_ref::<String>() {
                    format!("panic: {s}")
                } else {
                    "panic: unknown error".to_string()
                };
                set_error(&msg);
                $error_val
            }
        }
    }};
}

// ---------------------------------------------------------------------------
// Session wrapper
// ---------------------------------------------------------------------------

/// Opaque session handle passed to C code.
pub struct SessionHandle {
    session: atuin_client::session::Session,
    /// PID at session creation time. Used to detect fork() children where the
    /// tokio runtime is in a corrupted state and must not be used.
    creator_pid: u32,
    /// Cached NUL-terminated session UUID. Returning a pointer to this field
    /// avoids allocating (and leaking) a new CString on every UUID query.
    uuid: CString,
}

/// Check for fork: zsh forks for `$(...)`, `&`, pipelines and subshells, and
/// the child process inherits the tokio runtime in a corrupted state. Every
/// function that may touch the runtime must call this after validating `handle`.
macro_rules! guard_fork {
    ($handle:expr, $error_val:expr) => {
        if { &*$handle }.creator_pid != std::process::id() {
            set_error("refusing call in forked child process");
            return $error_val;
        }
    };
}

/// Convert a `CString` into caller-owned memory, unless it contains NUL bytes.
/// For search results (which may be arbitrary user input), truncate at the
/// first NUL instead of failing the whole call.
fn into_raw_or_truncate(value: String) -> Result<*mut c_char, ()> {
    match CString::new(value) {
        Ok(c_string) => Ok(c_string.into_raw()),
        Err(e) => {
            let pos = e.nul_position();
            let mut bytes = e.into_vec();
            bytes.truncate(pos);
            CString::new(bytes).map(|c| c.into_raw()).map_err(|_| ())
        }
    }
}

// ---------------------------------------------------------------------------
// Public C API
// ---------------------------------------------------------------------------

/// Create a new history session. `data_dir` is the Atuin data directory
/// (e.g. `~/.local/share/atuin`); pass NULL to use the default from
/// `ATUIN_DATA_DIR` / `XDG_DATA_HOME`.
///
/// Returns a non-null opaque pointer on success, or null on failure
/// (check `atuin_last_error`).
///
/// # Safety
/// `data_dir`, when non-null, must point to a valid NUL-terminated C string
/// for the duration of this call.
#[unsafe(no_mangle)]
pub extern "C" fn atuin_session_create(data_dir: *const c_char) -> *mut SessionHandle {
    ffi_guard!(
        {
            let dir: Option<PathBuf> = if data_dir.is_null() {
                // None makes Session::new follow the fully-resolved settings
                // (ATUIN_DATA_DIR / XDG_DATA_HOME / data_dir in config.toml).
                None
            } else {
                // SAFETY: data_dir was checked non-null; callers guarantee it is
                // a valid NUL-terminated UTF-8 C string for the duration of this call.
                match unsafe { CStr::from_ptr(data_dir) }.to_str() {
                    Ok(s) => Some(PathBuf::from(s)),
                    Err(_) => {
                        set_error("data_dir is not valid UTF-8");
                        return ptr::null_mut();
                    }
                }
            };

            let session = match atuin_client::session::Session::new(dir.as_deref()) {
                Ok(s) => s,
                Err(e) => {
                    set_error(&e.to_string());
                    return ptr::null_mut();
                }
            };

            let uuid = match CString::new(session.session_id_str().to_string()) {
                Ok(uuid) => uuid,
                Err(_) => {
                    set_error("session UUID contains a NUL byte");
                    return ptr::null_mut();
                }
            };
            let handle = Box::new(SessionHandle {
                session,
                creator_pid: std::process::id(),
                uuid,
            });
            Box::into_raw(handle)
        },
        ptr::null_mut()
    )
}

/// Destroy a session previously created with `atuin_session_create`.
///
/// Passing NULL is safe (no-op). The Session's `Drop` implementation shuts
/// down the tokio runtime before the database handles are released.
///
/// # Safety
/// `handle` must be NULL or a live pointer returned by
/// `atuin_session_create` that has not been destroyed yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_session_destroy(handle: *mut SessionHandle) {
    if handle.is_null() {
        return;
    }
    ffi_guard!(
        {
            if unsafe { &*handle }.creator_pid != std::process::id() {
                set_error(
                    "refusing to use session in forked child process \
                     (tokio runtime is invalid after fork)",
                );
                return;
            }
            // SAFETY: handle was created by atuin_session_create and has not
            // been destroyed before.
            unsafe {
                let _ = Box::from_raw(handle);
            }
        },
        ()
    );
}

/// Record a command start.
///
/// On success, returns 0 and writes a Rust-allocated UTF-8 history ID to
/// `*id_out`; the caller must free it with `atuin_free_string`.
/// On failure, returns <0, sets `*id_out` to NULL and stores a diagnostic
/// retrievable with `atuin_last_error`.
///
/// # Safety
/// `handle` must be a live session pointer or NULL. `command` must be a valid
/// NUL-terminated C string. `cwd`, `author` and `intent` must be NULL or valid
/// NUL-terminated C strings. `id_out` must be NULL or point to a writable
/// `char *` slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_history_start(
    handle: *mut SessionHandle,
    command: *const c_char,
    cwd: *const c_char,
    author: *const c_char,
    intent: *const c_char,
    id_out: *mut *mut c_char,
) -> c_int {
    ffi_guard!(
        {
            // Always reset the caller's output slot before any other
            // validation: on failure the caller may otherwise keep a stale
            // pointer from a previous successful call.
            if !id_out.is_null() {
                unsafe {
                    *id_out = ptr::null_mut();
                }
            }

            if handle.is_null() || command.is_null() || id_out.is_null() {
                set_error("atuin_history_start: null argument");
                return -1;
            }

            guard_fork!(handle, -1);
            let h = unsafe { &*handle };

            // Invalid UTF-8 or an embedded NUL cannot be represented in a C
            // string. `to_str` failure is treated as an empty string, matching
            // how `atuin` handles unrepresentable command data.
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
            let int = if intent.is_null() {
                None
            } else {
                unsafe { CStr::from_ptr(intent) }.to_str().ok()
            };

            let id = match h.session.history_start(cmd, cwd_str, auth, None, int) {
                Ok(id) => id,
                Err(e) => {
                    set_error(&e.to_string());
                    return -1;
                }
            };

            let id = match id {
                Some(id) => id,
                None => {
                    return 0;
                }
            };

            let id_ptr = match CString::new(id) {
                Ok(id) => id,
                Err(_) => {
                    set_error("history ID contains a NUL byte");
                    return -1;
                }
            };
            unsafe {
                *id_out = id_ptr.into_raw();
            }
            0
        },
        -1
    )
}

/// Finalise a command started with `atuin_history_start`.
///
/// When `sync` is non-zero, blocks until the database update completes and
/// returns 0 on success / <0 on error. When `sync` is 0, spawns the work on a
/// tokio multi-thread worker and returns immediately — matching the official
/// `(atuin history end ... &)` fire-and-forget behavior. Async errors are
/// logged by the Atuin session and are not reflected in the return value.
///
/// # Safety
/// `handle` must be a live session pointer or NULL, and `id` must be a valid
/// NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_history_end(
    handle: *mut SessionHandle,
    id: *const c_char,
    exit_code: i64,
    duration_ns: i64,
    sync: c_int,
) -> c_int {
    ffi_guard!(
        {
            if handle.is_null() || id.is_null() {
                set_error("atuin_history_end: null argument");
                return -1;
            }
            guard_fork!(handle, -1);
            let h = unsafe { &*handle };
            let id_str = unsafe { CStr::from_ptr(id) }
                .to_str()
                .unwrap_or("")
                .to_string();

            if sync != 0 {
                if let Err(e) = h.session.history_end(&id_str, exit_code, duration_ns) {
                    set_error(&e.to_string());
                    return -1;
                }
            } else {
                h.session.history_end_async(id_str, exit_code, duration_ns);
            }
            0
        },
        -1
    )
}

/// Search history by prefix. Writes results as newline-separated commands to
/// `*out` (caller must free with `atuin_free_string`).
///
/// `query` may be NULL (treated as the empty string). `limit <= 0` returns an
/// empty (but non-null on success) result.
///
/// # Safety
/// `handle` must be a live session pointer or NULL. `query` must be NULL or a
/// valid NUL-terminated C string. `out` must be NULL or point to a writable
/// `char *` slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_search_prefix(
    handle: *mut SessionHandle,
    query: *const c_char,
    limit: c_int,
    out: *mut *mut c_char,
) -> c_int {
    ffi_guard!(
        {
            if !out.is_null() {
                unsafe {
                    *out = ptr::null_mut();
                }
            }

            if handle.is_null() || out.is_null() {
                set_error("atuin_search_prefix: null argument");
                return -1;
            }
            guard_fork!(handle, -1);
            let h = unsafe { &*handle };
            let q = if query.is_null() {
                ""
            } else {
                unsafe { CStr::from_ptr(query) }.to_str().unwrap_or("")
            };

            let limit = if limit < 0 { 0 } else { limit as usize };
            let results = match h.session.search_prefix(q, limit) {
                Ok(r) => r,
                Err(e) => {
                    set_error(&e.to_string());
                    return -1;
                }
            };

            let output = results
                .iter()
                .map(|h| h.command.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let out_ptr = match into_raw_or_truncate(output) {
                Ok(ptr) => ptr,
                Err(()) => {
                    set_error("search result could not be encoded as a C string");
                    return -1;
                }
            };
            unsafe {
                *out = out_ptr;
            }
            0
        },
        -1
    )
}

/// Interactive TUI search over the in-process session's history.
///
/// Opens a full-screen ratatui UI on the controlling terminal, prefilled with
/// `query`. This is the native replacement for `atuin search --interactive`
/// and requires a controlling terminal (stdout may be redirected; output and
/// input then use `/dev/tty` / `CONOUT$`).
///
/// Return codes:
/// * `0`  — a command was selected; `*out` receives a Rust-allocated UTF-8
///   string (free with `atuin_free_string`). The string is prefixed with
///   `__atuin_accept__:` when the shell should execute it immediately (per
///   `enter_accept` config).
/// * `1`  — the user cancelled (Esc / Ctrl+C / Ctrl+G); `*out` is NULL and
///   the shell must leave its buffer unchanged.
/// * `<0` — error (no terminal, database failure, ...); check
///   `atuin_last_error`.
///
/// This call blocks the shell's main thread while the TUI is open, exactly
/// like launching the official interactive-search process. Raw mode and the
/// alternate screen are restored before returning, including on panic.
///
/// # Safety
/// `handle` must be a live session pointer or NULL. `query` must be NULL or a
/// valid NUL-terminated C string. `out` must be NULL or point to a writable
/// `char *` slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_search_interactive(
    handle: *mut SessionHandle,
    query: *const c_char,
    out: *mut *mut c_char,
) -> c_int {
    ffi_guard!(
        {
            if !out.is_null() {
                unsafe {
                    *out = ptr::null_mut();
                }
            }

            if handle.is_null() || out.is_null() {
                set_error("atuin_search_interactive: null argument");
                return -1;
            }
            guard_fork!(handle, -1);
            let h = unsafe { &*handle };
            let q = if query.is_null() {
                ""
            } else {
                unsafe { CStr::from_ptr(query) }.to_str().unwrap_or("")
            };

            match crate::tui::interactive_search(&h.session, q) {
                Ok(Some(selected)) => {
                    let out_ptr = match into_raw_or_truncate(selected) {
                        Ok(ptr) => ptr,
                        Err(()) => {
                            set_error("selected command could not be encoded as a C string");
                            return -1;
                        }
                    };
                    unsafe {
                        *out = out_ptr;
                    }
                    0
                }
                Ok(None) => 1,
                Err(e) => {
                    set_error(&format!("interactive search failed: {e:#}"));
                    -1
                }
            }
        },
        -1
    )
}

/// Free a string returned by `atuin_history_start`, `atuin_search_prefix` or
/// `atuin_search_interactive`. NULL is safe (no-op).
///
/// # Safety
/// `ptr` must be NULL or a pointer previously returned by this library exactly
/// once and not freed before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_free_string(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    ffi_guard!(
        {
            // SAFETY: contract requires ptr to have been returned by this
            // library exactly once and not freed before.
            unsafe {
                let _ = CString::from_raw(ptr);
            }
        },
        ()
    );
}

/// Return the session UUID.
///
/// The pointer is owned by the session and is valid until the session is
/// destroyed (or the next fork guard failure overwrites the error slot).
/// Do NOT free it.
///
/// # Safety
/// `handle` must be a live session pointer or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_session_uuid(handle: *mut SessionHandle) -> *const c_char {
    ffi_guard!(
        {
            if handle.is_null() {
                set_error("atuin_session_uuid: null argument");
                return ptr::null();
            }
            guard_fork!(handle, ptr::null());
            let h = unsafe { &*handle };
            h.uuid.as_ptr()
        },
        ptr::null()
    )
}

/// Return the library version as a static string.
///
/// The returned pointer is valid for the lifetime of the process. Do NOT free.
#[unsafe(no_mangle)]
pub extern "C" fn atuin_version() -> *const c_char {
    // A string literal has static storage duration and the trailing NUL is
    // included in the literal itself, so no LazyLock/allocation is needed.
    static VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
    VERSION.as_ptr().cast()
}

/// Return the last error message, or NULL when no error is set.
///
/// The pointer is valid until the next `atuin_*` FFI call that clears/sets the
/// error slot. Do NOT free it; copy immediately if the value must outlive the
/// next FFI call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atuin_last_error(out: *mut *mut c_char) {
    if out.is_null() {
        return;
    }
    let c_string = LAST_ERROR.lock().ok().and_then(|e| e.clone());
    // SAFETY: `out` points to a writable `char *` slot.
    unsafe {
        *out = match c_string {
            Some(c_string) => c_string.into_raw(),
            None => ptr::null_mut(),
        };
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::ptr;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // The exported functions are `unsafe extern "C"` to make the raw-pointer
    // safety contract explicit. These test-only safe wrappers keep the test
    // bodies focused on behavior while still validating the same code paths.
    fn session_create(data_dir: *const c_char) -> *mut SessionHandle {
        unsafe { super::atuin_session_create(data_dir) }
    }

    fn session_destroy(handle: *mut SessionHandle) {
        unsafe { super::atuin_session_destroy(handle) }
    }

    fn history_start(
        handle: *mut SessionHandle,
        command: *const c_char,
        cwd: *const c_char,
        author: *const c_char,
        intent: *const c_char,
        id_out: *mut *mut c_char,
    ) -> c_int {
        unsafe { super::atuin_history_start(handle, command, cwd, author, intent, id_out) }
    }

    fn history_end(
        handle: *mut SessionHandle,
        id: *const c_char,
        exit_code: i64,
        duration_ns: i64,
        sync: c_int,
    ) -> c_int {
        unsafe { super::atuin_history_end(handle, id, exit_code, duration_ns, sync) }
    }

    fn search_prefix(
        handle: *mut SessionHandle,
        query: *const c_char,
        limit: c_int,
        out: *mut *mut c_char,
    ) -> c_int {
        unsafe { super::atuin_search_prefix(handle, query, limit, out) }
    }

    fn search_interactive(
        handle: *mut SessionHandle,
        query: *const c_char,
        out: *mut *mut c_char,
    ) -> c_int {
        unsafe { super::atuin_search_interactive(handle, query, out) }
    }

    fn free_string(ptr: *mut c_char) {
        unsafe { super::atuin_free_string(ptr) }
    }

    fn session_uuid(handle: *mut SessionHandle) -> *const c_char {
        unsafe { super::atuin_session_uuid(handle) }
    }

    /// Create a session backed by a fresh temporary data directory.
    fn create_test_session() -> (*mut SessionHandle, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let path = CString::new(tmp.path().to_str().unwrap()).unwrap();
        let session = session_create(path.as_ptr());
        assert!(
            !session.is_null(),
            "session creation should succeed: {}",
            last_error_as_str().unwrap_or_else(|| "unknown".to_string())
        );
        (session, tmp)
    }

    /// Start a command in `session`, returning the caller-owned ID string.
    fn start_test_command(session: *mut SessionHandle, command: &str) -> String {
        let cmd = CString::new(command).unwrap();
        let cwd = CString::new("/tmp").unwrap();
        let mut id_ptr: *mut c_char = ptr::null_mut();
        let rc = history_start(
            session,
            cmd.as_ptr(),
            cwd.as_ptr(),
            ptr::null(),
            ptr::null(),
            &mut id_ptr,
        );
        assert_eq!(
            rc,
            0,
            "history_start failed: {}",
            last_error_as_str().unwrap_or_else(|| "unknown".to_string())
        );
        assert!(!id_ptr.is_null());
        let id = unsafe { CStr::from_ptr(id_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        free_string(id_ptr);
        id
    }

    fn last_error_as_str() -> Option<String> {
        let mut ptr: *mut c_char = ptr::null_mut();
        unsafe { atuin_last_error(&mut ptr) };
        if ptr.is_null() {
            None
        } else {
            Some(
                unsafe { CStr::from_ptr(ptr) }
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }

    #[test]
    fn test_session_create_destroy() {
        let (session, _tmp) = create_test_session();
        session_destroy(session);
    }

    #[test]
    fn test_destroy_null_is_safe() {
        session_destroy(ptr::null_mut());
    }

    #[test]
    fn test_free_null_is_safe() {
        free_string(ptr::null_mut());
    }

    #[test]
    fn test_version_is_static_nonempty_string() {
        let v = atuin_version();
        assert!(!v.is_null());
        let s = unsafe { CStr::from_ptr(v) }.to_str().unwrap();
        assert_eq!(s, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn test_last_error_lifecycle() {
        let mut out: *mut c_char = ptr::null_mut();
        let rc = history_start(
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            &mut out,
        );
        assert!(rc < 0);
        assert!(out.is_null(), "failed call must reset output pointer");
        let err = last_error_as_str();
        assert!(err.is_some(), "error should be set after a failed call");

        // A successful call starts by clearing the previous error.
        let (session, _tmp) = create_test_session();
        let id = start_test_command(session, "echo clear-error");
        assert!(
            last_error_as_str().is_none(),
            "successful FFI call should clear the previous error"
        );
        history_end(session, CString::new(id).unwrap().as_ptr(), 0, 0, 1);
        session_destroy(session);
    }

    #[test]
    fn test_history_start_rejects_null_arguments() {
        let (session, _tmp) = create_test_session();

        let mut id_ptr: *mut c_char = ptr::null_mut();
        let rc = history_start(
            session,
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            &mut id_ptr,
        );
        assert!(rc < 0);
        assert!(id_ptr.is_null());

        let rc = history_start(
            session,
            CString::new("echo").unwrap().as_ptr(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null_mut(),
        );
        assert!(rc < 0);

        session_destroy(session);
    }

    #[test]
    fn test_history_end_rejects_null_arguments() {
        let (session, _tmp) = create_test_session();
        assert!(history_end(ptr::null_mut(), ptr::null(), 0, 0, 1) < 0);
        assert!(history_end(session, ptr::null(), 0, 0, 1) < 0);
        session_destroy(session);
    }

    #[test]
    fn test_history_roundtrip_sync() {
        let (session, _tmp) = create_test_session();
        let id = start_test_command(session, "echo hello from ffi");

        let id_c = CString::new(id).unwrap();
        let rc = history_end(session, id_c.as_ptr(), 0, 1_000_000, 1);
        assert_eq!(
            rc,
            0,
            "sync history_end failed: {}",
            last_error_as_str().unwrap_or_else(|| "unknown".to_string())
        );

        let query = CString::new("echo hello").unwrap();
        let mut out: *mut c_char = ptr::null_mut();
        assert_eq!(search_prefix(session, query.as_ptr(), 10, &mut out), 0);
        assert!(!out.is_null());
        let results = unsafe { CStr::from_ptr(out) }.to_str().unwrap();
        assert!(results.contains("echo hello from ffi"), "got: {results}");
        free_string(out);
        session_destroy(session);
    }

    #[test]
    fn test_history_end_async_completes_on_worker_thread() {
        let (session, _tmp) = create_test_session();
        let id = start_test_command(session, "echo worker-thread-async");
        let id_c = CString::new(id.as_str()).unwrap();

        // sync=0 is the precmd fast path: it must return before the DB update
        // has necessarily finished, and the multi-thread runtime must complete
        // the update on a worker thread without another FFI call to drive it.
        assert_eq!(history_end(session, id_c.as_ptr(), 7, 123_456, 0), 0);

        let handle = unsafe { &*session };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let updated = handle.session.get_history(&id).unwrap();
            if updated
                .as_ref()
                .is_some_and(|h| h.exit == 7 && h.duration == 123_456)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fire-and-forget history_end did not complete on a worker thread"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        session_destroy(session);
    }

    #[test]
    fn test_history_end_persists_record_store() {
        let (session, tmp) = create_test_session();
        let id = start_test_command(session, "echo record-store-test");
        let id_c = CString::new(id.as_str()).unwrap();
        assert_eq!(history_end(session, id_c.as_ptr(), 0, 42, 1), 0);

        let records = tmp.path().join("records.db");
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

        session_destroy(session);
    }

    #[test]
    fn test_history_end_async_does_not_crash() {
        let (session, _tmp) = create_test_session();
        let id = start_test_command(session, "echo async-atuin-test");
        let id_c = CString::new(id.as_str()).unwrap();
        assert_eq!(history_end(session, id_c.as_ptr(), 0, 0, 0), 0);

        // Calling sync end with the same ID immediately afterwards must also be
        // safe: whichever update wins sets duration, and the other skips.
        let id_c = CString::new(id).unwrap();
        assert_eq!(history_end(session, id_c.as_ptr(), 0, 0, 1), 0);
        session_destroy(session);
    }

    #[test]
    fn test_search_limits_results() {
        let (session, _tmp) = create_test_session();

        let first = start_test_command(session, "alpha first command");
        let second = start_test_command(session, "alpha second command");
        assert_ne!(first, second);

        let query = CString::new("alpha").unwrap();
        let mut out: *mut c_char = ptr::null_mut();
        assert_eq!(search_prefix(session, query.as_ptr(), 1, &mut out), 0);
        assert!(!out.is_null());
        let results = unsafe { CStr::from_ptr(out) }.to_str().unwrap();
        assert_eq!(
            results.lines().count(),
            1,
            "limit=1 should return one line: {results}"
        );
        free_string(out);

        let mut out: *mut c_char = ptr::null_mut();
        assert_eq!(search_prefix(session, query.as_ptr(), 0, &mut out), 0);
        assert!(!out.is_null());
        assert!(unsafe { CStr::from_ptr(out) }.to_bytes().is_empty());
        free_string(out);

        session_destroy(session);
    }

    #[test]
    fn test_search_rejects_null_arguments() {
        let (session, _tmp) = create_test_session();

        let mut out: *mut c_char = ptr::null_mut();
        assert!(search_prefix(ptr::null_mut(), ptr::null(), 1, &mut out) < 0);
        assert!(out.is_null());

        let stale: *mut c_char = CString::new("stale").unwrap().into_raw();
        assert!(search_prefix(session, ptr::null(), 1, ptr::null_mut()) < 0);
        // A null out-pointer is invalid, so the caller keeps the stale value;
        // free it to avoid leaking test memory.
        free_string(stale);

        session_destroy(session);
    }

    #[test]
    fn test_search_interactive_rejects_null_arguments_without_terminal() {
        // Argument validation happens before any terminal I/O, so this test is
        // safe even under a TTY: with valid arguments the function would open
        // the full-screen UI and block.
        let mut out: *mut c_char = CString::new("stale").unwrap().into_raw();
        assert!(
            search_interactive(ptr::null_mut(), ptr::null(), &mut out) < 0,
            "null handle must be rejected before the TUI starts"
        );
        assert!(out.is_null(), "failed call must reset the output slot");
        free_string(out);

        let (session, _tmp) = create_test_session();
        assert!(
            search_interactive(session, ptr::null(), ptr::null_mut()) < 0,
            "null out pointer must be rejected before the TUI starts"
        );
        session_destroy(session);
    }

    #[test]
    fn test_search_missing_query_returns_empty() {
        let (session, _tmp) = create_test_session();
        start_test_command(session, "definitely-not-found-command");

        let query = CString::new("no-such-prefix").unwrap();
        let mut out: *mut c_char = ptr::null_mut();
        assert_eq!(search_prefix(session, query.as_ptr(), 10, &mut out), 0);
        assert!(!out.is_null());
        assert!(unsafe { CStr::from_ptr(out) }.to_bytes().is_empty());
        free_string(out);
        session_destroy(session);
    }

    #[test]
    fn test_session_uuid_is_stable() {
        let (session, _tmp) = create_test_session();
        let first = session_uuid(session);
        let second = session_uuid(session);
        assert!(!first.is_null());
        assert!(!second.is_null());
        assert_eq!(first, second, "UUID pointer/value must be stable");
        let uuid = unsafe { CStr::from_ptr(first) }.to_str().unwrap();
        assert!(!uuid.is_empty());
        session_destroy(session);
    }

    #[test]
    fn test_session_uuid_rejects_null() {
        assert!(session_uuid(ptr::null_mut()).is_null());
        assert!(last_error_as_str().is_some());
    }

    #[test]
    fn test_search_with_null_query_matches_empty_prefix() {
        let (session, _tmp) = create_test_session();
        start_test_command(session, "null-query-prefix-test");

        let mut out: *mut c_char = ptr::null_mut();
        assert_eq!(search_prefix(session, ptr::null(), 20, &mut out), 0);
        assert!(!out.is_null());
        let results = unsafe { CStr::from_ptr(out) }.to_str().unwrap();
        assert!(!results.is_empty());
        free_string(out);
        session_destroy(session);
    }

    #[test]
    fn test_command_trailing_newline_is_trimmed() {
        let (session, _tmp) = create_test_session();
        let id = start_test_command(session, "echo trimmed-command\n");
        let id_c = CString::new(id).unwrap();
        assert_eq!(history_end(session, id_c.as_ptr(), 0, 0, 1), 0);

        let query = CString::new("echo trimmed-command").unwrap();
        let mut out: *mut c_char = ptr::null_mut();
        assert_eq!(search_prefix(session, query.as_ptr(), 10, &mut out), 0);
        let results = unsafe { CStr::from_ptr(out) }.to_str().unwrap();
        assert!(results.contains("echo trimmed-command"), "got: {results}");
        free_string(out);
        session_destroy(session);
    }
}
