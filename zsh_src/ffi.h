/*
 * ffi.h — C declarations for the atuin-ffi library.
 *
 * Include this header in the zsh module shim (module.c) or in C test
 * harnesses. It must stay in sync with rust_src/src/ffi.rs.
 *
 * # Error protocol
 *
 * Every fallible atuin_* export returns `char *`:
 *   * NULL means success.
 *   * A non-NULL value is a heap-allocated, NUL-terminated UTF-8 error string.
 *     The caller owns it and must release it with atuin_free().
 *
 * # Single session
 *
 * The library holds at most one session per loaded instance, because Atuin's
 * client layer keeps process-global state (resolved data directory, meta
 * store). There is no session handle:
 *   * atuin_init() is idempotent: it succeeds and keeps the existing session
 *     when one is already active.
 *   * atuin_shutdown() is idempotent and must be called before the
 *     library is unloaded.
 *   * Every other export fails with "session is not initialized" when none
 *     exists.
 */
#ifndef ATUIN_FFI_H
#define ATUIN_FFI_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Session lifecycle.
 * atuin_init() resolves the Atuin data directory from the user's
 * configuration (ATUIN_DATA_DIR, XDG, or data_dir in config.toml), exactly like
 * the official CLI. Returns NULL on success, or an allocated error string.
 * Calling it while a session is already active is a successful no-op. */
char *atuin_init(void);
/* atuin_shutdown() shuts down the runtime/SQLite pools. Calling it with
 * no active session is a successful no-op, so it is safe to call on cleanup. */
char *atuin_shutdown(void);

/* History recording.
 * atuin_history_start writes a Rust-allocated ID to *id_out (which may stay
 * NULL when Atuin's filters drop the command) and returns NULL on success.
 * The caller must free *id_out with atuin_free.
 * author / author_kind / intent mirror `atuin history start --author`,
 * `--author-kind` and `--intent`; author_kind is "user" or "agent"
 * (case-insensitive). NULL performs the same best-effort probe as the CLI. */
char *atuin_history_start(const char *command, const char *cwd,
                          const char *author, const char *author_kind,
                          const char *intent, char **id_out);
/* sync=1 blocks until done and returns an error string on failure; sync=0 is
 * fire-and-forget and returns NULL once the work is scheduled. */
char *atuin_history_end(const char *id, long long exit_code,
                        long long duration_ns, int sync);

/* Non-interactive search options. Mirrors the upstream `atuin search` flags.
 * String pointers are borrowed for the duration of the call; authors/shells
 * are arrays of count NUL-terminated C strings; exits/exclude_exits are arrays
 * of count 64-bit exit codes (repeatable `--exit` / `--exclude-exit`). An
 * empty array (count 0) means no restriction. */
typedef enum atuin_search_mode {
  ATUIN_SEARCH_MODE_AUTO = 0,
  ATUIN_SEARCH_MODE_PREFIX = 1,
  ATUIN_SEARCH_MODE_FULLTEXT = 2,
  ATUIN_SEARCH_MODE_FUZZY = 3,
  ATUIN_SEARCH_MODE_DAEMON_FUZZY = 4,
} atuin_search_mode_t;

typedef enum atuin_filter_mode {
  ATUIN_FILTER_MODE_AUTO = 0,
  ATUIN_FILTER_MODE_GLOBAL = 1,
  ATUIN_FILTER_MODE_HOST = 2,
  ATUIN_FILTER_MODE_SESSION = 3,
  ATUIN_FILTER_MODE_DIRECTORY = 4,
  ATUIN_FILTER_MODE_WORKSPACE = 5,
  ATUIN_FILTER_MODE_SESSION_PRELOAD = 6,
} atuin_filter_mode_t;

typedef struct atuin_search_options {
  const char *query;
  int search_mode;
  int filter_mode;
  const char *cwd;
  const char *exclude_cwd;
  const long long *exits;
  size_t exit_count;
  const long long *exclude_exits;
  size_t exclude_exit_count;
  const char *before;
  const char *after;
  int has_limit;
  long long limit;
  int has_offset;
  long long offset;
  int reverse;
  int include_duplicates;
  const char *const *authors;
  size_t author_count;
  const char *const *shells;
  size_t shell_count;
} atuin_search_options_t;

/* Generic search. Returns NULL and writes newline-separated command results to
 * *out on success (free *out with atuin_free). */
char *atuin_search(const atuin_search_options_t *options, char **out);

/* Convenience prefix search. Returns NULL and writes newline-separated results
 * to *out (free with atuin_free). limit <= 0 returns no results. */
char *atuin_search_prefix(const char *query, int limit, char **out);

/* Interactive full-screen search TUI (official `atuin search -i` replacement).
 * Requires a controlling terminal; blocks until a selection is made.
 * `shell_up_key_binding` / `keymap_mode` mirror the official
 * `--shell-up-key-binding` / `--keymap-mode` flags; keymap_mode is one of the
 * ATUIN_KEYMAP_MODE_* values below.
 * Returns NULL on success (including a user cancel). When a command was
 * selected, *out receives it (free with atuin_free; may be prefixed with
 * __atuin_accept__:); when the user cancelled, *out stays NULL. A non-NULL
 * return value is an error string. */
#define ATUIN_KEYMAP_MODE_AUTO 0
#define ATUIN_KEYMAP_MODE_EMACS 1
#define ATUIN_KEYMAP_MODE_VIM_NORMAL 2
#define ATUIN_KEYMAP_MODE_VIM_INSERT 3

char *atuin_search_interactive(const char *query, int shell_up_key_binding,
                               int keymap_mode, char **out);

/* Session statistics: a snapshot of the counters the in-process session has
 * accumulated (history writes and searches). Returns NULL and writes the
 * snapshot to *out on success. */
typedef struct atuin_stats {
  unsigned long long history_starts;
  unsigned long long history_ends_sync;
  unsigned long long history_ends_async;
  unsigned long long search_calls;
  unsigned long long search_prefix_calls;
  unsigned long long interactive_search_calls;
  unsigned long long interactive_selections;
  unsigned long long interactive_cancels;
  unsigned long long in_flight_history_ends;
  unsigned long long uptime_secs;
} atuin_stats_t;

char *atuin_stats(atuin_stats_t *out);

/* Memory management. Passing NULL is safe. */
void atuin_free(char *ptr);

/* Metadata.
 * atuin_session_uuid returns NULL on success and writes a session-owned
 * pointer to *out (do NOT free); on failure it returns an error string and
 * sets *out to NULL.
 * atuin_version returns a process-lifetime pointer (do NOT free). */
char *atuin_session_uuid(const char **out);
const char *atuin_version(void);

#ifdef __cplusplus
}
#endif

#endif /* ATUIN_FFI_H */
