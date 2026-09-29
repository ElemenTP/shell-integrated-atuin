/*
 * module.c — zsh loadable module for in-process Atuin history recording.
 *
 * Dynamically links libatuin_ffi.so (see ffi.h). Both shared objects
 * must be in the same directory; the module is built with $ORIGIN /
 * @loader_path rpath so the FFI library resolves next to the module.
 *
 * Like the starship/zoxide native modules, the builtins take their input from
 * zsh parameters and write their results back to zsh parameters. This keeps the
 * C shim tiny and avoids fragile argv parsing. The only exceptions are
 * `atuin_stats` and `atuin_version`, which accept the convenience flags `-v`
 * (verbose) and `-q` (quiet) for their printed summary; all of their data
 * still travels through parameters.
 *
 * Builtins:
 *   atuin_history_start  — reads $ATUIN_HISTORY_COMMAND / $ATUIN_HISTORY_CWD /
 *                          $ATUIN_HISTORY_AUTHOR / $ATUIN_HISTORY_AUTHOR_KIND /
 *                          $ATUIN_HISTORY_INTENT,
 *                          writes $ATUIN_HISTORY_ID
 *   atuin_history_end    — reads $ATUIN_HISTORY_ID / $ATUIN_HISTORY_EXIT /
 *                          $ATUIN_HISTORY_DURATION_NS / $ATUIN_HISTORY_SYNC
 *   atuin_search         — reads $ATUIN_SEARCH_* options, writes
 *                          $ATUIN_SEARCH_RESULT
 *   atuin_search_prefix  — reads $ATUIN_SEARCH_QUERY / $ATUIN_SEARCH_LIMIT,
 *                          writes $ATUIN_SEARCH_RESULT (autosuggest fast path)
 *   atuin_search_interactive — reads $ATUIN_SEARCH_QUERY /
 *                              $ATUIN_SEARCH_SHELL_UP_KEY_BINDING /
 *                              $ATUIN_SEARCH_KEYMAP_MODE,
 *                              writes $ATUIN_SEARCH_SELECTED
 *   atuin_stats          — accepts -v/-q, writes $ATUIN_STATS_*
 *   atuin_session_id     — writes $ATUIN_SESSION
 *   atuin_version        — accepts -q, writes $ATUIN_VERSION
 */

#define MODULE
#define IMPORTING_MODULE_zshQsmain 1
#include "zsh.mdh"

#include "ffi.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ------------------------------------------------------------------ */
/* Module metadata                                                    */
/* ------------------------------------------------------------------ */
#define MODNAME "atuin_native"
#define BUILTIN_ATUIN_HISTORY_START "atuin_history_start"
#define BUILTIN_ATUIN_HISTORY_END "atuin_history_end"
#define BUILTIN_ATUIN_SEARCH "atuin_search"
#define BUILTIN_ATUIN_SEARCH_PREFIX "atuin_search_prefix"
#define BUILTIN_ATUIN_SEARCH_INTERACTIVE "atuin_search_interactive"
#define BUILTIN_ATUIN_STATS "atuin_stats"
#define BUILTIN_ATUIN_SESSION_ID "atuin_session_id"
#define BUILTIN_ATUIN_VERSION "atuin_version"

/* Forward declarations */
// clang-format off
static int bin_atuin_history_start(char *name, char **argv, Options ops, int func);
static int bin_atuin_history_end(char *name, char **argv, Options ops, int func);
static int bin_atuin_session_id(char *name, char **argv, Options ops, int func);
static int bin_atuin_search(char *name, char **argv, Options ops, int func);
static int bin_atuin_search_prefix(char *name, char **argv, Options ops, int func);
static int bin_atuin_search_interactive(char *name, char **argv, Options ops, int func);
static int bin_atuin_stats(char *name, char **argv, Options ops, int func);
static int bin_atuin_version(char *name, char **argv, Options ops, int func);
// clang-format on

/* Builtin table */
// clang-format off
static struct builtin bintab[] = {
    BUILTIN(BUILTIN_ATUIN_HISTORY_START, 0, bin_atuin_history_start, 0, 0, 0, NULL, NULL),
    BUILTIN(BUILTIN_ATUIN_HISTORY_END,   0, bin_atuin_history_end,   0, 0, 0, NULL, NULL),
    BUILTIN(BUILTIN_ATUIN_SEARCH,        0, bin_atuin_search,        0, 0, 0, NULL, NULL),
    BUILTIN(BUILTIN_ATUIN_SEARCH_PREFIX, 0, bin_atuin_search_prefix, 0, 0, 0, NULL, NULL),
    BUILTIN(BUILTIN_ATUIN_SEARCH_INTERACTIVE, 0, bin_atuin_search_interactive, 0, 0, 0, NULL, NULL),
    BUILTIN(BUILTIN_ATUIN_STATS,         0, bin_atuin_stats,         0, -1, 0, NULL, NULL),
    BUILTIN(BUILTIN_ATUIN_SESSION_ID,    0, bin_atuin_session_id,    0, 0, 0, NULL, NULL),
    BUILTIN(BUILTIN_ATUIN_VERSION,       0, bin_atuin_version,       0, -1, 0, NULL, NULL),
};
// clang-format on

static struct features module_features = {
    bintab, sizeof(bintab) / sizeof(*bintab), /* builtins */
    NULL,   0,                                /* conditions */
    NULL,   0,                                /* math functions */
    NULL,   0,                                /* parameter definitions */
    0,                                        /* n_abstract */
};

/* ------------------------------------------------------------------ */
/* Parameter helpers                                                   */
/* ------------------------------------------------------------------ */

/*
 * Read a zsh scalar parameter in UNMETAFIED form.
 *
 * getsparam_u() returns a pointer into a STATIC buffer that is invalidated by
 * the next getsparam_u() call, so callers must duplicate the result. A NULL or
 * empty parameter is treated as "unset" (returns NULL).
 */
static char *get_str_param(const char *name) {
  char *val = getsparam_u((char *)name);
  if (!val || !*val)
    return NULL;
  return ztrdup(val);
}

/* Read a zsh array parameter into a freshly allocated, unmetafied copy.
 *
 * A scalar parameter is accepted as a one-element array, so repeatable
 * options (--author / --shell / --exit / --exclude-exit) still work when the
 * user assigns a single value instead of an array. Caller must release the
 * result with freearray() when *len > 0. */
static char **get_arr_param(const char *name, size_t *len) {
  char **arr = getaparam((char *)name);

  if (!arr) {
    /* Not an array (or unset): fall back to the scalar value. getsparam()
     * returns a metafied pointer into the parameters table, so duplicate it
     * immediately; the copy is unmetafied below like the array elements. */
    char *scalar = getsparam((char *)name);
    if (!scalar || !*scalar) {
      *len = 0;
      return NULL;
    }

    char **one = (char **)zalloc(2 * sizeof(char *));
    one[0] = ztrdup(scalar);
    unmetafy(one[0], NULL);
    one[1] = NULL;

    *len = 1;
    return one;
  }

  size_t n = arrlen(arr);
  if (n == 0) {
    *len = 0;
    return arr;
  }

  char **copy = zarrdup(arr);
  for (size_t i = 0; i < n; i++)
    unmetafy(copy[i], NULL);

  *len = n;
  return copy;
}

/* Write a Rust-returned UTF-8 string into a zsh parameter.
 * The raw string must be metafied for zsh's internal storage. */
static void set_str_param(const char *name, const char *val) {
  if (!val)
    return;
  setsparam((char *)name, ztrdup_metafy(val));
}

/* Report and free an allocated FFI error string. */
static void report_ffi_error(char *err, const char *operation) {
  zwarnnam(MODNAME, "%s: %s", operation, err ? err : "unknown error");
  if (err)
    atuin_free(err);
}

/* Parse a strict base-10 64-bit integer. */
static int parse_i64(const char *text, long long *out) {
  char *end = NULL;
  if (!text || !*text)
    return -1;

  errno = 0;
  long long value = strtoll(text, &end, 10);
  if (errno == ERANGE || end == text || *end != '\0')
    return -1;

  *out = (long long)value;
  return 0;
}

/* Read an optional integer parameter. Returns:
 *   0 = unset/empty
 *   1 = set and parsed into *out
 *  -1 = set but invalid
 */
static int optional_i64_param(const char *name, int *has, long long *out) {
  char *text = get_str_param(name);
  if (!text) {
    if (has)
      *has = 0;
    return 0;
  }

  int rc = parse_i64(text, out);
  zsfree(text);
  if (rc != 0)
    return -1;

  *has = 1;
  return 1;
}

/* Read an array of base-10 64-bit integers (repeatable `--exit` flags).
 * Returns 0 on success (possibly empty) and -1 when an element is invalid.
 * The caller frees a non-empty result with zfree(out, *count *
 * sizeof(long long)). */
static int get_i64_arr_param(const char *name, long long **out, size_t *count) {
  *out = NULL;
  *count = 0;

  size_t n = 0;
  char **items = get_arr_param(name, &n);
  if (n == 0)
    /* `get_arr_param` returns the live zsh array for an empty parameter;
     * nothing is allocated and nothing must be freed. */
    return 0;

  long long *values = (long long *)zalloc(n * sizeof(long long));
  for (size_t i = 0; i < n; i++) {
    if (parse_i64(items[i], &values[i]) != 0) {
      freearray(items);
      zfree(values, n * sizeof(long long));
      return -1;
    }
  }

  freearray(items);
  *out = values;
  *count = n;
  return 0;
}

/* Read a boolean parameter. Unset, "0", "false" and "no" are false. */
static int bool_param(const char *name) {
  char *text = get_str_param(name);
  if (!text)
    return 0;

  int value = strcmp(text, "0") != 0 && strcmp(text, "false") != 0 &&
              strcmp(text, "no") != 0;
  zsfree(text);
  return value;
}

static int search_mode_from_str(const char *text, int *out) {
  if (!text || !*text || strcmp(text, "auto") == 0) {
    *out = ATUIN_SEARCH_MODE_AUTO;
    return 0;
  }
  if (strcmp(text, "prefix") == 0) {
    *out = ATUIN_SEARCH_MODE_PREFIX;
  } else if (strcmp(text, "fulltext") == 0 || strcmp(text, "full-text") == 0) {
    *out = ATUIN_SEARCH_MODE_FULLTEXT;
  } else if (strcmp(text, "fuzzy") == 0) {
    *out = ATUIN_SEARCH_MODE_FUZZY;
  } else if (strcmp(text, "daemon-fuzzy") == 0) {
    *out = ATUIN_SEARCH_MODE_DAEMON_FUZZY;
  } else {
    return -1;
  }
  return 0;
}

static int filter_mode_from_str(const char *text, int *out) {
  if (!text || !*text || strcmp(text, "auto") == 0) {
    *out = ATUIN_FILTER_MODE_AUTO;
    return 0;
  }
  if (strcmp(text, "global") == 0) {
    *out = ATUIN_FILTER_MODE_GLOBAL;
  } else if (strcmp(text, "host") == 0) {
    *out = ATUIN_FILTER_MODE_HOST;
  } else if (strcmp(text, "session") == 0) {
    *out = ATUIN_FILTER_MODE_SESSION;
  } else if (strcmp(text, "directory") == 0) {
    *out = ATUIN_FILTER_MODE_DIRECTORY;
  } else if (strcmp(text, "workspace") == 0) {
    *out = ATUIN_FILTER_MODE_WORKSPACE;
  } else if (strcmp(text, "session-preload") == 0) {
    *out = ATUIN_FILTER_MODE_SESSION_PRELOAD;
  } else {
    return -1;
  }
  return 0;
}

/* ------------------------------------------------------------------ */
/* Builtin: atuin_history_start                                       */
/*                                                                   */
/* Inputs:                                                            */
/*   ATUIN_HISTORY_COMMAND  — command line to record (required)       */
/*   ATUIN_HISTORY_CWD      — working directory (default: empty)      */
/*   ATUIN_HISTORY_AUTHOR   — optional author                         */
/*   ATUIN_HISTORY_AUTHOR_KIND — optional "user"/"agent"              */
/*   ATUIN_HISTORY_INTENT   — optional intent                         */
/* Output:                                                            */
/*   ATUIN_HISTORY_ID       — new history ID                          */
/* ------------------------------------------------------------------ */

static int bin_atuin_history_start(UNUSED(char *name), UNUSED(char **argv),
                                   UNUSED(Options ops), UNUSED(int func)) {

  /* --- Read zsh params --- */
  char *command = get_str_param("ATUIN_HISTORY_COMMAND");
  char *cwd = get_str_param("ATUIN_HISTORY_CWD");
  char *author = get_str_param("ATUIN_HISTORY_AUTHOR");
  char *author_kind = get_str_param("ATUIN_HISTORY_AUTHOR_KIND");
  char *intent = get_str_param("ATUIN_HISTORY_INTENT");

  char *id_out = NULL;
  char *err = atuin_history_start(command, cwd ? cwd : "", author, author_kind,
                                  intent, &id_out);
  zsfree(command);
  zsfree(cwd);
  zsfree(author);
  zsfree(author_kind);
  zsfree(intent);

  if (err) {
    report_ffi_error(err, BUILTIN_ATUIN_HISTORY_START);
    return 1;
  }

  /* A successful call can legitimately produce no ID (e.g. when Atuin's
   * exclusion filters drop the command); expose that as an empty ID. */
  set_str_param("ATUIN_HISTORY_ID", id_out ? id_out : "");
  if (id_out) {
    atuin_free(id_out);
  }
  return 0;
}

/* ------------------------------------------------------------------ */
/* Builtin: atuin_history_end                                         */
/*                                                                   */
/* Inputs:                                                            */
/*   ATUIN_HISTORY_ID        — ID from atuin_history_start            */
/*   ATUIN_HISTORY_EXIT      — exit code (default 0)                  */
/*   ATUIN_HISTORY_DURATION_NS — duration in ns (default 0)           */
/*   ATUIN_HISTORY_SYNC      — 1 to block until persisted, else async */
/* ------------------------------------------------------------------ */

static int bin_atuin_history_end(UNUSED(char *name), UNUSED(char **argv),
                                 UNUSED(Options ops), UNUSED(int func)) {
  char *id = get_str_param("ATUIN_HISTORY_ID");
  int has_exit = 0;
  long long exit_code = 0;
  int rc_exit = optional_i64_param("ATUIN_HISTORY_EXIT", &has_exit, &exit_code);
  if (rc_exit < 0) {
    zwarnnam(MODNAME, "invalid ATUIN_HISTORY_EXIT");
    zsfree(id);
    return 1;
  }

  int has_duration = 0;
  long long duration_ns = 0;
  int rc_duration = optional_i64_param("ATUIN_HISTORY_DURATION_NS",
                                       &has_duration, &duration_ns);
  /* The FFI boundary is signed (long long / int64_t) and only accepts a
   * non-negative duration; 0 asks Atuin to infer it from the start
   * timestamp. Reject negative values here so shell errors stay visible. */
  if (rc_duration < 0 || duration_ns < 0) {
    zwarnnam(MODNAME, "invalid ATUIN_HISTORY_DURATION_NS");
    zsfree(id);
    return 1;
  }

  int sync = bool_param("ATUIN_HISTORY_SYNC");

  char *err =
      atuin_history_end(id, (long long)exit_code, (long long)duration_ns, sync);
  zsfree(id);

  if (err) {
    /* With sync=0 the FFI layer never produces an error: async failures
     * are logged on the tokio worker and deliberately do not surface to
     * the prompt. */
    report_ffi_error(err, BUILTIN_ATUIN_HISTORY_END);
    return 1;
  }

  return 0;
}

/* ------------------------------------------------------------------ */
/* Search option helpers                                               */
/* ------------------------------------------------------------------ */

/* Parse the non-interactive search options from ATUIN_SEARCH_* parameters. */
static int read_search_options(atuin_search_options_t *opts) {
  memset(opts, 0, sizeof(*opts));

  char *search_mode = get_str_param("ATUIN_SEARCH_MODE");
  if (search_mode) {
    if (search_mode_from_str(search_mode, &opts->search_mode) != 0) {
      zwarnnam(MODNAME, "invalid ATUIN_SEARCH_MODE: %s", search_mode);
      zsfree(search_mode);
      return -1;
    }
    zsfree(search_mode);
  }

  char *filter_mode = get_str_param("ATUIN_SEARCH_FILTER_MODE");
  if (filter_mode) {
    if (filter_mode_from_str(filter_mode, &opts->filter_mode) != 0) {
      zwarnnam(MODNAME, "invalid ATUIN_SEARCH_FILTER_MODE: %s", filter_mode);
      zsfree(filter_mode);
      return -1;
    }
    zsfree(filter_mode);
  }

  int has = 0;

  /* Repeatable `--exit` / `--exclude-exit`: zsh arrays of integer codes. */
  long long *exits = NULL;
  size_t exit_count = 0;
  if (get_i64_arr_param("ATUIN_SEARCH_EXITS", &exits, &exit_count) != 0) {
    zwarnnam(MODNAME, "invalid ATUIN_SEARCH_EXITS");
    return -1;
  }

  long long *exclude_exits = NULL;
  size_t exclude_exit_count = 0;
  if (get_i64_arr_param("ATUIN_SEARCH_EXCLUDE_EXITS", &exclude_exits,
                        &exclude_exit_count) != 0) {
    if (exit_count > 0) {
      zfree(exits, exit_count * sizeof(long long));
    }
    zwarnnam(MODNAME, "invalid ATUIN_SEARCH_EXCLUDE_EXITS");
    return -1;
  }

  /* Only publish the arrays once both parsed successfully. */
  opts->exits = exits;
  opts->exit_count = exit_count;
  opts->exclude_exits = exclude_exits;
  opts->exclude_exit_count = exclude_exit_count;

  int rc = optional_i64_param("ATUIN_SEARCH_LIMIT", &has, &opts->limit);
  if (rc < 0 || (has && opts->limit < 0)) {
    zwarnnam(MODNAME, "invalid ATUIN_SEARCH_LIMIT");
    return -1;
  }
  opts->has_limit = has;

  rc = optional_i64_param("ATUIN_SEARCH_OFFSET", &has, &opts->offset);
  if (rc < 0 || (has && opts->offset < 0)) {
    zwarnnam(MODNAME, "invalid ATUIN_SEARCH_OFFSET");
    return -1;
  }
  opts->has_offset = has;

  opts->reverse = bool_param("ATUIN_SEARCH_REVERSE");
  opts->include_duplicates = bool_param("ATUIN_SEARCH_INCLUDE_DUPLICATES");

  opts->query = get_str_param("ATUIN_SEARCH_QUERY");
  opts->cwd = get_str_param("ATUIN_SEARCH_CWD");
  opts->exclude_cwd = get_str_param("ATUIN_SEARCH_EXCLUDE_CWD");
  opts->before = get_str_param("ATUIN_SEARCH_BEFORE");
  opts->after = get_str_param("ATUIN_SEARCH_AFTER");

  size_t author_count = 0;
  char **authors = get_arr_param("ATUIN_SEARCH_AUTHORS", &author_count);
  opts->authors = (const char *const *)authors;
  opts->author_count = author_count;

  size_t shell_count = 0;
  char **shells = get_arr_param("ATUIN_SEARCH_SHELLS", &shell_count);
  opts->shells = (const char *const *)shells;
  opts->shell_count = shell_count;

  return 0;
}

static void free_search_options(atuin_search_options_t *opts) {
  zsfree((char *)opts->query);
  zsfree((char *)opts->cwd);
  zsfree((char *)opts->exclude_cwd);
  zsfree((char *)opts->before);
  zsfree((char *)opts->after);
  if (opts->author_count > 0 && opts->authors) {
    freearray((char **)opts->authors);
  }
  if (opts->shell_count > 0 && opts->shells) {
    freearray((char **)opts->shells);
  }
  if (opts->exit_count > 0) {
    zfree((long long *)opts->exits, opts->exit_count * sizeof(long long));
  }
  if (opts->exclude_exit_count > 0) {
    zfree((long long *)opts->exclude_exits,
          opts->exclude_exit_count * sizeof(long long));
  }
}

/* ------------------------------------------------------------------ */
/* Builtin: atuin_search                                              */
/*                                                                   */
/* Reads ATUIN_SEARCH_* parameters (query, modes, filters, limits)    */
/* and writes newline-separated matching commands to                 */
/* ATUIN_SEARCH_RESULT.                                              */
/* ------------------------------------------------------------------ */

static int bin_atuin_search(UNUSED(char *name), UNUSED(char **argv),
                            UNUSED(Options ops), UNUSED(int func)) {
  atuin_search_options_t opts;
  if (read_search_options(&opts) != 0) {
    free_search_options(&opts);
    unsetparam((char *)"ATUIN_SEARCH_RESULT");
    return 1;
  }

  char *out = NULL;
  char *err = atuin_search(&opts, &out);
  free_search_options(&opts);

  if (err) {
    unsetparam((char *)"ATUIN_SEARCH_RESULT");
    report_ffi_error(err, BUILTIN_ATUIN_SEARCH);
    return 1;
  }

  set_str_param("ATUIN_SEARCH_RESULT", out ? out : "");
  if (out) {
    atuin_free(out);
  }
  return 0;
}

/* ------------------------------------------------------------------ */
/* Builtin: atuin_search_prefix                                       */
/*                                                                   */
/* Autosuggest fast path. Reads ATUIN_SEARCH_QUERY and                */
/* ATUIN_SEARCH_LIMIT (default 1), calls the dedicated               */
/* Session::search_prefix path (official `atuin search --cmd-only     */
/* --author '$all-user' --limit N --search-mode prefix`) and writes   */
/* newline-separated matches to ATUIN_SEARCH_RESULT.                  */
/* ------------------------------------------------------------------ */

static int bin_atuin_search_prefix(UNUSED(char *name), UNUSED(char **argv),
                                   UNUSED(Options ops), UNUSED(int func)) {
  char *query = get_str_param("ATUIN_SEARCH_QUERY");

  int has_limit = 0;
  long long limit = 1;
  int rc_limit = optional_i64_param("ATUIN_SEARCH_LIMIT", &has_limit, &limit);
  if (rc_limit < 0) {
    zwarnnam(MODNAME, "invalid ATUIN_SEARCH_LIMIT");
    zsfree(query);
    unsetparam((char *)"ATUIN_SEARCH_RESULT");
    return 1;
  }
  if (!has_limit) {
    limit = 1;
  }
  limit = MAX(0, limit);
  limit = MIN(INT32_MAX, limit);

  char *out = NULL;
  char *err = atuin_search_prefix(query ? query : "", (int)limit, &out);
  zsfree(query);

  if (err) {
    unsetparam((char *)"ATUIN_SEARCH_RESULT");
    report_ffi_error(err, BUILTIN_ATUIN_SEARCH_PREFIX);
    return 1;
  }

  set_str_param("ATUIN_SEARCH_RESULT", out ? out : "");
  if (out) {
    atuin_free(out);
  }
  return 0;
}

/* Map ATUIN_SEARCH_KEYMAP_MODE to the FFI enum. Unknown values are "auto". */
static int keymap_mode_from_str(const char *value) {
  if (!value || !*value) {
    return ATUIN_KEYMAP_MODE_AUTO;
  }
  if (strcmp(value, "emacs") == 0) {
    return ATUIN_KEYMAP_MODE_EMACS;
  }
  if (strcmp(value, "vim-normal") == 0) {
    return ATUIN_KEYMAP_MODE_VIM_NORMAL;
  }
  if (strcmp(value, "vim-insert") == 0) {
    return ATUIN_KEYMAP_MODE_VIM_INSERT;
  }
  return ATUIN_KEYMAP_MODE_AUTO;
}

/* ------------------------------------------------------------------ */
/* Builtin: atuin_search_interactive                                  */
/*                                                                   */
/* Reads the initial query from ATUIN_SEARCH_QUERY, the UpArrow flag  */
/* from ATUIN_SEARCH_SHELL_UP_KEY_BINDING and the vi keymap from      */
/* ATUIN_SEARCH_KEYMAP_MODE, then writes the selected command to      */
/* ATUIN_SEARCH_SELECTED.                                            */
/*                                                                   */
/* Return status: 0 = selected ($ATUIN_SEARCH_SELECTED set),         */
/*                1 = cancelled ($ATUIN_SEARCH_SELECTED empty),      */
/*                2 = error.                                         */
/* ------------------------------------------------------------------ */

static int bin_atuin_search_interactive(UNUSED(char *name), UNUSED(char **argv),
                                        UNUSED(Options ops), UNUSED(int func)) {
  char *query = get_str_param("ATUIN_SEARCH_QUERY");
  char *keymap = get_str_param("ATUIN_SEARCH_KEYMAP_MODE");
  int shell_up = bool_param("ATUIN_SEARCH_SHELL_UP_KEY_BINDING");
  int keymap_mode = keymap_mode_from_str(keymap);
  zsfree(keymap);

  char *out = NULL;
  char *err =
      atuin_search_interactive(query ? query : "", shell_up, keymap_mode, &out);
  zsfree(query);

  if (err) {
    set_str_param("ATUIN_SEARCH_SELECTED", "");
    report_ffi_error(err, BUILTIN_ATUIN_SEARCH_INTERACTIVE);
    return 2;
  }

  /* The widget reads $ATUIN_SEARCH_SELECTED directly — no command
   * substitution and therefore no fork. A NULL out means the user
   * cancelled; the builtin keeps returning 1 for that case. */
  set_str_param("ATUIN_SEARCH_SELECTED", out ? out : "");
  if (out) {
    atuin_free(out);
    return 0;
  }
  return 1;
}

/* ------------------------------------------------------------------ */
/* Builtin: atuin_stats                                               */
/*                                                                   */
/* Takes the optional flags -v (verbose) / -q (quiet), writes the    */
/* ATUIN_STATS_* integer parameters and (unless quiet) prints a       */
/* summary, like starship_stats.                                     */
/* ------------------------------------------------------------------ */

static int bin_atuin_stats(UNUSED(char *name), char **argv, UNUSED(Options ops),
                           UNUSED(int func)) {
  int verbose = 0, quiet = 0;
  while (*argv) {
    if (strcmp(*argv, "-v") == 0)
      verbose = 1;
    else if (strcmp(*argv, "-q") == 0)
      quiet = 1;
    argv++;
  }

  atuin_stats_t st;
  memset(&st, 0, sizeof(st));
  char *err = atuin_stats(&st);
  if (err) {
    report_ffi_error(err, BUILTIN_ATUIN_STATS);
    return 1;
  }

  char buf[32];
#define SET_INT(name, val)                                                     \
  do {                                                                         \
    snprintf(buf, sizeof(buf), "%llu", (unsigned long long)(val));             \
    setsparam((char *)(name), ztrdup(buf));                                    \
  } while (0)

  SET_INT("ATUIN_STATS_HISTORY_STARTS", st.history_starts);
  SET_INT("ATUIN_STATS_HISTORY_ENDS_SYNC", st.history_ends_sync);
  SET_INT("ATUIN_STATS_HISTORY_ENDS_ASYNC", st.history_ends_async);
  SET_INT("ATUIN_STATS_SEARCH_CALLS", st.search_calls);
  SET_INT("ATUIN_STATS_SEARCH_PREFIX_CALLS", st.search_prefix_calls);
  SET_INT("ATUIN_STATS_INTERACTIVE_SEARCH_CALLS", st.interactive_search_calls);
  SET_INT("ATUIN_STATS_INTERACTIVE_SELECTIONS", st.interactive_selections);
  SET_INT("ATUIN_STATS_INTERACTIVE_CANCELS", st.interactive_cancels);
  SET_INT("ATUIN_STATS_IN_FLIGHT_HISTORY_ENDS", st.in_flight_history_ends);
  SET_INT("ATUIN_STATS_UPTIME_SECS", st.uptime_secs);

#undef SET_INT

  if (!quiet) {
    printf("atuin_native session stats (uptime %llus):\n", st.uptime_secs);
    printf("  history: starts=%llu ends_sync=%llu ends_async=%llu "
           "in_flight=%llu\n",
           st.history_starts, st.history_ends_sync, st.history_ends_async,
           st.in_flight_history_ends);
    printf("  searches: total=%llu prefix=%llu interactive=%llu\n",
           st.search_calls, st.search_prefix_calls,
           st.interactive_search_calls);

    if (verbose) {
      printf("  interactive: selections=%llu cancels=%llu\n",
             st.interactive_selections, st.interactive_cancels);
    }
  }

  return 0;
}

/* ------------------------------------------------------------------ */
/* Builtin: atuin_session_id                                         */
/* ------------------------------------------------------------------ */

static int bin_atuin_session_id(UNUSED(char *name), UNUSED(char **argv),
                                UNUSED(Options ops), UNUSED(int func)) {
  const char *id = NULL;
  char *err = atuin_session_uuid(&id);
  if (err) {
    report_ffi_error(err, BUILTIN_ATUIN_SESSION_ID);
    return 1;
  }
  if (!id) {
    zwarnnam(MODNAME, "atuin_session_uuid returned no UUID");
    return 1;
  }

  set_str_param("ATUIN_SESSION", id);
  printf("%s\n", id);
  return 0;
}

/* ------------------------------------------------------------------ */
/* Builtin: atuin_version                                            */
/*                                                                   */
/* Takes the optional flag -q (quiet), writes $ATUIN_VERSION and     */
/* (unless quiet) prints it.                                         */
/* ------------------------------------------------------------------ */

static int bin_atuin_version(UNUSED(char *name), char **argv,
                             UNUSED(Options ops), UNUSED(int func)) {
  int quiet = 0;
  while (*argv) {
    if (strcmp(*argv, "-q") == 0)
      quiet = 1;
    argv++;
  }

  const char *version = atuin_version();
  version = version ? version : "unknown";
  setsparam((char *)"ATUIN_VERSION", ztrdup(version));

  if (!quiet) {
    printf("%s\n", version);
  }

  return 0;
}

/* ------------------------------------------------------------------ */
/* zsh module entry points                                           */
/* ------------------------------------------------------------------ */

/**/
int setup_(UNUSED(Module m)) { return 0; }

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
int boot_(UNUSED(Module m)) {
  /* The native session resolves the Atuin data directory from the user's
   * configuration (ATUIN_DATA_DIR, XDG, or config.toml) exactly like the
   * official CLI, so no directory is passed here. */
  char *err = atuin_init();
  if (err) {
    zwarnnam(MODNAME, "failed to create session: %s", err);
    atuin_free(err);
    return 1;
  }
  return 0;
}

/**/
int cleanup_(Module m) {
  /* Tear down the tokio runtime before dlclose unmaps this module. */
  char *err = atuin_shutdown();
  if (err) {
    zwarnnam(MODNAME, "failed to destroy session: %s", err);
    atuin_free(err);
  }

  /* Disable all features before teardown. */
  return setfeatureenables(m, &module_features, NULL);
}

/**/
int finish_(UNUSED(Module m)) { return 0; }
