/*
 * atuin_ffi.h — C declarations for the atuin-ffi library.
 *
 * Include this header in the zsh module shim (atuin_module.c) or in C test
 * harnesses. It must stay in sync with rust_src/src/ffi.rs.
 */
#ifndef ATUIN_FFI_H
#define ATUIN_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque session handle. */
typedef struct atuin_session atuin_session_t;

/* Session lifecycle.
 * atuin_session_create(NULL) uses the default Atuin data directory. */
atuin_session_t *atuin_session_create(const char *data_dir);
void             atuin_session_destroy(atuin_session_t *s);

/* History recording. Returns 0 on success, <0 on error.
 * atuin_history_start writes a Rust-allocated string to *id_out and resets the
 * slot to NULL on failure. The caller must free *id_out with
 * atuin_free_string. */
int  atuin_history_start(atuin_session_t *s, const char *command, const char *cwd,
                          const char *author, const char *intent, char **id_out);
/* sync=1 blocks until done and returns the error code; sync=0 is
 * fire-and-forget. */
int  atuin_history_end(atuin_session_t *s, const char *id, int64_t exit_code,
                        int64_t duration_ns, int sync);

/* Search history by prefix. Writes newline-separated results to *out
 * (caller must free with atuin_free_string). limit <= 0 returns no results. */
int  atuin_search_prefix(atuin_session_t *s, const char *query, int limit,
                          char **out);

/* Interactive full-screen search TUI (official `atuin search -i` replacement).
 * Requires a controlling terminal; blocks until a selection is made.
 * Return: 0 = *out receives the selected command (free with
 * atuin_free_string; may be prefixed with __atuin_accept__:), 1 = cancelled
 * (*out stays NULL), <0 = error (check atuin_last_error). */
int  atuin_search_interactive(atuin_session_t *s, const char *query,
                               char **out);

/* Memory management. Passing NULL is safe. */
void atuin_free_string(char *ptr);

/* Metadata.
 * atuin_session_uuid returns a session-owned pointer (do NOT free).
 * atuin_version returns a process-lifetime pointer (do NOT free).
 * atuin_last_error returns a library-owned pointer that is valid until the
 * next atuin_* FFI call which clears or replaces the error slot. Copy it
 * immediately if the value must outlive the next FFI call. */
const char *atuin_session_uuid(atuin_session_t *s);
const char *atuin_version(void);
const char *atuin_last_error(void);

#ifdef __cplusplus
}
#endif

#endif /* ATUIN_FFI_H */
