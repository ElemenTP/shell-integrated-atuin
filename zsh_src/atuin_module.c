/*
 * atuin_module.c — zsh loadable module for in-process Atuin history recording.
 *
 * Dynamically links libatuin_ffi.so (see atuin_ffi.h). Both shared objects
 * must be in the same directory; the module is built with $ORIGIN /
 * @loader_path rpath so the FFI library resolves next to the module.
 *
 * Builtins:
 *   atuin_history_start  — record command start, returns history ID
 *   atuin_history_end    — record command end (sync or fire-and-forget)
 *   atuin_search         — prefix search, result in $ATUIN_SEARCH_RESULT
 *   atuin_search_interactive — full-screen TUI search, result in
 *                              $ATUIN_SEARCH_SELECTED
 *   atuin_session_id     — print session UUID
 *   atuin_version        — print library version
 */

#define MODULE
#define IMPORTING_MODULE_zshQsmain 1
#include "zsh.mdh"

#include "atuin_ffi.h"
#include "atuin_builtin_util.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define MODNAME "atuin_native"

/* Forward declarations */
static int bin_atuin_history_start(char *nam, char **args, Options ops, int func);
static int bin_atuin_history_end(char *nam, char **args, Options ops, int func);
static int bin_atuin_session_id(char *nam, char **args, Options ops, int func);
static int bin_atuin_search(char *nam, char **args, Options ops, int func);
static int bin_atuin_search_interactive(char *nam, char **args, Options ops, int func);
static int bin_atuin_version(char *nam, char **args, Options ops, int func);

/* Builtin table */
static struct builtin bintab[] = {
    BUILTIN("atuin_history_start", 0, bin_atuin_history_start, 1, 2, 0, NULL, NULL),
    BUILTIN("atuin_history_end",   0, bin_atuin_history_end,   2, -1, 0, NULL, NULL),
    BUILTIN("atuin_search",        0, bin_atuin_search,        1, 2, 0, NULL, NULL),
    BUILTIN("atuin_search_interactive", 0, bin_atuin_search_interactive, 0, 1, 0, NULL, NULL),
    BUILTIN("atuin_session_id",    0, bin_atuin_session_id,    0, 0, 0, NULL, NULL),
    BUILTIN("atuin_version",       0, bin_atuin_version,       0, 0, 0, NULL, NULL),
};

static struct features module_features = {
    bintab, sizeof(bintab) / sizeof(*bintab),
    NULL, 0, NULL, 0, NULL, 0, 0,
};

/* ------------------------------------------------------------------ */
/* Session state                                                      */
/* ------------------------------------------------------------------ */

static atuin_session_t *g_session = NULL;

/* ------------------------------------------------------------------ */
/* Helpers                                                             */
/* ------------------------------------------------------------------ */

/*
 * Copy a Rust-returned UTF-8 string into a zsh parameter. The string must be
 * metafied for zsh's internal storage, otherwise bytes above 0x80 (all
 * multi-byte UTF-8 characters) get corrupted by zsh's metafication round
 * trip. META_DUP tells metafy() to allocate and return a new string.
 */
static void set_str_param(const char *name, char *val) {
    const char *text = val ? val : "";
    setsparam((char *)name, metafy((char *)text, strlen(text), META_DUP));
}

/* Report an FFI failure using the Rust-side last-error string.
 * atuin_last_error returns a library-owned pointer that stays valid until the
 * next FFI call, so it is safe to consume here before any other FFI call. */
static void report_ffi_error(const char *operation) {
    const char *err = atuin_last_error();
    zwarnnam(MODNAME, "%s failed: %s", operation,
             err ? err : "unknown error");
}

/* ------------------------------------------------------------------ */
/* Builtin: atuin_history_start "command" [cwd]                      */
/* ------------------------------------------------------------------ */

static int
bin_atuin_history_start(char *nam, char **args, Options ops, int func)
{
    (void)nam; (void)ops; (void)func;

    if (!g_session) {
        zwarnnam(MODNAME, "session not initialized");
        return 1;
    }

    const char *command = args[0];
    const char *cwd     = args[1] ? args[1] : "";

    char *id_out = NULL;
    int rc = atuin_history_start(g_session, command, cwd, NULL, NULL, &id_out);
    if (rc != 0 || !id_out) {
        report_ffi_error("atuin_history_start");
        return 1;
    }

    /* Write both a zsh parameter (no command substitution/fork needed) and
     * stdout for callers that still want `id=$(...)` (which the fork guard
     * will reject — prefer the parameter form). */
    set_str_param("ATUIN_HISTORY_ID", id_out);
    printf("%s", id_out);
    atuin_free_string(id_out);
    return 0;
}

/* ------------------------------------------------------------------ */
/* Builtin: atuin_history_end <id> <exit> [duration_ns] [--sync]     */
/*                                                                  */
/* Default (no --sync) is the official `(atuin history end ... &)`  */
/* equivalent: the FFI layer spawns the update on a tokio           */
/* multi-thread worker and returns immediately.                     */
/* ------------------------------------------------------------------ */

static int
bin_atuin_history_end(char *nam, char **args, Options ops, int func)
{
    (void)nam; (void)ops; (void)func;

    if (!g_session) {
        zwarnnam(MODNAME, "session not initialized");
        return 1;
    }

    const char *id_str = args[0];

    long exit_code = 0;
    if (atuin_parse_long(args[1], &exit_code) != 0) {
        zwarnnam(MODNAME, "invalid exit code: %s", args[1]);
        return 1;
    }

    atuin_end_options_t opts;
    if (atuin_parse_end_options(args + 2, &opts) != 0) {
        zwarnnam(MODNAME, "invalid duration option: %s", args[2]);
        return 1;
    }

    int rc = atuin_history_end(g_session, id_str, (int64_t)exit_code,
                               (int64_t)opts.duration_ns, opts.sync);
    if (rc != 0 && opts.sync) {
        report_ffi_error("atuin_history_end");
        return 1;
    }

    /* Keep async errors non-fatal: they are logged on the tokio worker and
     * deliberately do not surface to the prompt. */
    return 0;
}

/* ------------------------------------------------------------------ */
/* Builtin: atuin_search <query> [limit]                             */
/* ------------------------------------------------------------------ */

static int
bin_atuin_search(char *nam, char **args, Options ops, int func)
{
    (void)nam; (void)ops; (void)func;

    if (!g_session) {
        zwarnnam(MODNAME, "session not initialized");
        return 1;
    }

    const char *query = args[0];
    int limit = atuin_parse_limit(args[1], 10, 1000);
    if (limit < 0) {
        zwarnnam(MODNAME, "invalid search limit: %s", args[1]);
        return 1;
    }

    char *out = NULL;
    int rc = atuin_search_prefix(g_session, query, limit, &out);
    if (rc != 0 || !out) {
        report_ffi_error("atuin_search");
        return 1;
    }

    printf("%s\n", out);
    set_str_param("ATUIN_SEARCH_RESULT", out);
    atuin_free_string(out);
    return 0;
}

/* ------------------------------------------------------------------ */
/* Builtin: atuin_search_interactive [query]                         */
/*                                                                  */
/* Opens the in-process full-screen search TUI on the controlling    */
/* terminal (the official `atuin search -i` replacement). Blocks    */
/* until the user selects a command or cancels; raw mode and the    */
/* alternate screen are restored by the FFI layer.                  */
/*                                                                  */
/* Return status: 0 = selected ($ATUIN_SEARCH_SELECTED set),        */
/*                1 = cancelled (parameter set to empty string),    */
/*                2 = error.                                        */
/* ------------------------------------------------------------------ */

static int
bin_atuin_search_interactive(char *nam, char **args, Options ops, int func)
{
    (void)nam; (void)ops; (void)func;

    if (!g_session) {
        zwarnnam(MODNAME, "session not initialized");
        return 2;
    }

    const char *query = args[0] ? args[0] : "";

    char *out = NULL;
    int rc = atuin_search_interactive(g_session, query, &out);
    if (rc < 0) {
        report_ffi_error("atuin_search_interactive");
        return 2;
    }

    /* The widget reads $ATUIN_SEARCH_SELECTED directly — no command
     * substitution and therefore no fork. */
    set_str_param("ATUIN_SEARCH_SELECTED", out ? out : "");
    atuin_free_string(out);
    return rc;
}

/* ------------------------------------------------------------------ */
/* Builtin: atuin_session_id                                         */
/* ------------------------------------------------------------------ */

static int
bin_atuin_session_id(char *nam, char **args, Options ops, int func)
{
    (void)nam; (void)args; (void)ops; (void)func;

    if (!g_session) {
        zwarnnam(MODNAME, "session not initialized");
        return 1;
    }

    const char *id = atuin_session_uuid(g_session);
    if (!id) {
        report_ffi_error("atuin_session_uuid");
        return 1;
    }

    set_str_param("ATUIN_SESSION", (char *)id);
    printf("%s\n", id);
    return 0;
}

/* ------------------------------------------------------------------ */
/* Builtin: atuin_version                                            */
/* ------------------------------------------------------------------ */

static int
bin_atuin_version(char *nam, char **args, Options ops, int func)
{
    (void)nam; (void)args; (void)ops; (void)func;

    const char *version = atuin_version();
    if (!version) version = "unknown";

    set_str_param("ATUIN_NATIVE_VERSION", (char *)version);
    printf("%s\n", version);
    return 0;
}

/* ------------------------------------------------------------------ */
/* zsh module entry points                                           */
/* ------------------------------------------------------------------ */

/**/
int setup_(Module m) { (void)m; return 0; }

/**/
int features_(Module m, char ***features) {
    *features = featuresarray(m, &module_features);
    return 0;
}

/**/
int enables_(Module m, int **enables) {
    return handlefeatures(m, &module_features, enables);
}

/**/
int boot_(Module m) {
    (void)m;

    /* An empty ATUIN_DATA_DIR means "use the default location", matching
     * atuin's own behavior more closely than passing an empty path. */
    const char *data_dir = getenv("ATUIN_DATA_DIR");
    if (data_dir && *data_dir == '\0') {
        data_dir = NULL;
    }

    g_session = atuin_session_create(data_dir);
    if (!g_session) {
        const char *err = atuin_last_error();
        zwarnnam(MODNAME, "failed to create session: %s",
                 err ? err : "unknown error");
        return 1;
    }
    return 0;
}

/**/
int cleanup_(Module m) {
    /* Tear down the tokio runtime before dlclose unmaps this module. */
    if (g_session) {
        atuin_session_destroy(g_session);
        g_session = NULL;
    }

    /* Disable all features before teardown. */
    return setfeatureenables(m, &module_features, NULL);
}

/**/
int finish_(Module m) { (void)m; return 0; }
