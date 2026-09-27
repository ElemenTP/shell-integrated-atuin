#define _GNU_SOURCE

/*
 * ffi_smoke.c — dlopen-based smoke/system test for the atuin-ffi C API.
 *
 * It loads libatuin_ffi.so at runtime exactly like zsh's zmodload would, then
 * exercises every exported function: lifecycle, history round-trip, search,
 * metadata, error handling, NULL safety, and the fork guard.
 *
 * The library exposes a single process-global session and no handle; see
 * zsh_src/ffi.h.
 *
 * Error protocol: every fallible `atuin_*` export returns `char *`; NULL means
 * success and a non-NULL value is an allocated error string the caller frees
 * with atuin_free().
 *
 * Build:
 *   cc -std=c11 -Wall -Wextra -Werror -o ffi_smoke ffi_smoke.c -ldl
 *
 * Usage:
 *   ./ffi_smoke [path/to/libatuin_ffi.so]
 */

#include <dlfcn.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

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
  const int64_t *exits;
  size_t exit_count;
  const int64_t *exclude_exits;
  size_t exclude_exit_count;
  const char *before;
  const char *after;
  int has_limit;
  int64_t limit;
  int has_offset;
  int64_t offset;
  int reverse;
  int include_duplicates;
  const char *const *authors;
  size_t author_count;
  const char *const *shells;
  size_t shell_count;
} atuin_search_options_t;

typedef struct atuin_stats {
  uint64_t history_starts;
  uint64_t history_ends_sync;
  uint64_t history_ends_async;
  uint64_t search_calls;
  uint64_t search_prefix_calls;
  uint64_t interactive_search_calls;
  uint64_t interactive_selections;
  uint64_t interactive_cancels;
  uint64_t in_flight_history_ends;
  uint64_t uptime_secs;
} atuin_stats_t;

#define ATUIN_KEYMAP_MODE_AUTO 0
#define ATUIN_KEYMAP_MODE_EMACS 1
#define ATUIN_KEYMAP_MODE_VIM_NORMAL 2
#define ATUIN_KEYMAP_MODE_VIM_INSERT 3

static void *lib_handle = NULL;
static char *(*fn_session_create)(void);
static char *(*fn_session_destroy)(void);
static char *(*fn_history_start)(const char *, const char *, const char *,
                                 const char *, const char *, char **);
static char *(*fn_history_end)(const char *, int64_t, int64_t, int);
static char *(*fn_search)(const atuin_search_options_t *, char **);
static char *(*fn_search_prefix)(const char *, int, char **);
static char *(*fn_search_interactive)(const char *, int, int, char **);
static char *(*fn_stats)(atuin_stats_t *);
static void (*fn_free)(char *);
static char *(*fn_session_uuid)(const char **);
static const char *(*fn_version)(void);

static int passed = 0, failed = 0;
static char data_dir[] = "/tmp/atuin-ffi-smoke-XXXXXX";
/* Isolated Atuin data/config locations, derived from data_dir in main(). */
static char atuin_data[128];
static char config_dir[128];

#define TEST(name) printf("  %-46s ", name)
#define PASS()                                                                 \
  do {                                                                         \
    printf("PASS\n");                                                          \
    passed++;                                                                  \
  } while (0)
#define FAIL(msg)                                                              \
  do {                                                                         \
    printf("FAIL: %s\n", msg);                                                 \
    failed++;                                                                  \
  } while (0)

static int load_library(const char *path) {
  lib_handle = dlopen(path, RTLD_NOW | RTLD_GLOBAL);
  if (!lib_handle) {
    fprintf(stderr, "dlopen: %s\n", dlerror());
    return -1;
  }
#define L(sym, name)                                                           \
  *(void **)(&fn_##sym) = dlsym(lib_handle, name);                             \
  if (!fn_##sym) {                                                             \
    fprintf(stderr, "dlsym(%s): %s\n", name, dlerror());                       \
    return -1;                                                                 \
  }
  L(session_create, "atuin_init");
  L(session_destroy, "atuin_shutdown");
  L(history_start, "atuin_history_start");
  L(history_end, "atuin_history_end");
  L(search, "atuin_search");
  L(search_prefix, "atuin_search_prefix");
  L(search_interactive, "atuin_search_interactive");
  L(stats, "atuin_stats");
  L(free, "atuin_free");
  L(session_uuid, "atuin_session_uuid");
  L(version, "atuin_version");
#undef L
  return 0;
}

/* Read and free an allocated error string, returning the message in `buf`. */
static void take_error(char *err, char *buf, size_t buflen) {
  const char *msg = err ? err : "unknown error";
  snprintf(buf, buflen, "%s", msg);
  if (err) {
    fn_free(err);
  }
}

/* Create the global session, reporting the returned error string on failure. */
static int create_session(void) {
  char *err = fn_session_create();
  if (err) {
    char msg[512];
    take_error(err, msg, sizeof(msg));
    printf("  (create_session error: %s)\n", msg);
    return -1;
  }
  return 0;
}

/* Destroy the global session, reporting the error string on failure. */
static int destroy_session(void) {
  char *err = fn_session_destroy();
  if (err) {
    char msg[512];
    take_error(err, msg, sizeof(msg));
    printf("  (destroy_session error: %s)\n", msg);
    return -1;
  }
  return 0;
}

/* Start a command with explicit author fields (mirrors `atuin history start`
 * `--author` / `--author-kind` / `--intent`). */
static int start_command_full(const char *command, const char *author,
                              const char *author_kind, const char *intent,
                              char **id_out) {
  char *id = NULL;
  char *err =
      fn_history_start(command, "/tmp", author, author_kind, intent, &id);
  if (err) {
    char msg[512];
    take_error(err, msg, sizeof(msg));
    printf("  (history_start error: %s)\n", msg);
    return -1;
  }
  *id_out = id;
  return 0;
}

/* Run a command start, returning 0 on success and writing the ID to *id_out. */
static int start_command(const char *command, char **id_out) {
  return start_command_full(command, NULL, NULL, NULL, id_out);
}

/* Run a sync history end with an explicit exit code. */
static int end_command_exit(const char *id, int64_t exit_code) {
  char *err = fn_history_end(id, exit_code, 0, 1);
  if (err) {
    char msg[512];
    take_error(err, msg, sizeof(msg));
    printf("  (history_end error: %s)\n", msg);
    return -1;
  }
  return 0;
}

/* Run a sync history end with exit 0, returning 0 on success. */
static int end_command_sync(const char *id) { return end_command_exit(id, 0); }

/* Run a generic prefix search with repeatable include/exclude exit filters. */
static int search_with_exits(const char *query, const int64_t *exits,
                             size_t exit_count, const int64_t *exclude_exits,
                             size_t exclude_exit_count, char **out) {
  atuin_search_options_t opts;
  memset(&opts, 0, sizeof(opts));
  opts.query = query;
  opts.search_mode = ATUIN_SEARCH_MODE_PREFIX;
  opts.exits = exits;
  opts.exit_count = exit_count;
  opts.exclude_exits = exclude_exits;
  opts.exclude_exit_count = exclude_exit_count;

  char *result = NULL;
  char *err = fn_search(&opts, &result);
  if (err) {
    char msg[512];
    take_error(err, msg, sizeof(msg));
    printf("  (search error: %s)\n", msg);
    return -1;
  }
  *out = result;
  return 0;
}

/* Run a prefix search, returning 0 on success and writing results to *out. */
static int prefix_search(const char *query, int limit, char **out) {
  char *result = NULL;
  char *err = fn_search_prefix(query, limit, &result);
  if (err) {
    char msg[512];
    take_error(err, msg, sizeof(msg));
    printf("  (search_prefix error: %s)\n", msg);
    return -1;
  }
  *out = result;
  return 0;
}

int main(int argc, char **argv) {
  const char *libpath =
      (argc > 1) ? argv[1] : "rust_src/target/release/libatuin_ffi.so";
  printf("Atuin FFI Smoke Test\nLibrary: %s\n\n", libpath);

  if (mkdtemp(data_dir) == NULL) {
    perror("mkdtemp");
    return 1;
  }

  /* atuin_init takes no data-directory argument: the session follows
   * the same settings resolution as the official CLI. Point the settings tree
   * at the temp directory so the test never reads or writes the developer's
   * real ~/.config/atuin or data directory. The explicit config.toml makes the
   * resolved location deterministic from the very first session. */
  snprintf(config_dir, sizeof(config_dir), "%s/config", data_dir);
  snprintf(atuin_data, sizeof(atuin_data), "%s/atuin", data_dir);
  if (mkdir(config_dir, 0700) != 0 && errno != EEXIST) {
    perror("mkdir");
    return 1;
  }
  {
    char config_path[sizeof(config_dir) + 32];
    snprintf(config_path, sizeof(config_path), "%s/config.toml", config_dir);
    FILE *config = fopen(config_path, "w");
    if (!config) {
      perror("fopen");
      return 1;
    }
    fprintf(config, "data_dir = \"%s\"\n", atuin_data);
    fclose(config);
  }
  setenv("ATUIN_CONFIG_DIR", config_dir, 1);
  setenv("ATUIN_DATA_DIR", atuin_data, 1);

  if (load_library(libpath) != 0) {
    return 1;
  }

  /* 1. version */
  TEST("version");
  {
    const char *v = fn_version();
    if (v && strlen(v) > 0)
      PASS();
    else
      FAIL("bad version");
  }

  /* 2. create/destroy */
  TEST("create/destroy");
  {
    if (create_session() == 0 && destroy_session() == 0)
      PASS();
  }

  /* 3. destroy without a session is an idempotent no-op */
  TEST("destroy without session");
  {
    char *err = fn_session_destroy();
    if (err == NULL)
      PASS();
    else {
      fn_free(err);
      FAIL("destroy without a session returned an error");
    }
  }

  /* 3b. calls without a session are rejected */
  TEST("calls without session rejected");
  {
    char *id = NULL;
    char *err = fn_history_start("echo no-session", "/tmp", NULL, NULL, NULL, &id);
    if (err && strstr(err, "session is not initialized") != NULL && id == NULL) {
      fn_free(err);
      PASS();
    } else {
      fn_free(err);
      fn_free(id);
      FAIL("call without a session was not rejected");
    }
  }

  /* 4. free(NULL) */
  TEST("free(NULL)");
  {
    fn_free(NULL);
    PASS();
  }

  /* 5. session UUID */
  TEST("session UUID stable");
  {
    if (create_session() == 0) {
      const char *a = NULL;
      const char *b = NULL;
      char *err_a = fn_session_uuid(&a);
      char *err_b = fn_session_uuid(&b);
      if (!err_a && !err_b && a && b && strcmp(a, b) == 0)
        PASS();
      else
        FAIL("UUID changed, was null, or returned an error");
      fn_free(err_a);
      fn_free(err_b);
      destroy_session();
    }
  }

  /* 5b. a second init while one is active is an idempotent no-op */
  TEST("create twice is idempotent");
  {
    if (create_session() == 0) {
      const char *before = NULL;
      char *uuid_err = fn_session_uuid(&before);
      char *err = fn_session_create();
      const char *after = NULL;
      char *uuid_err2 = fn_session_uuid(&after);
      if (!uuid_err && !uuid_err2 && !err && before && after &&
          strcmp(before, after) == 0) {
        PASS();
      } else {
        FAIL("second init failed or replaced the active session");
      }
      fn_free(uuid_err);
      fn_free(uuid_err2);
      fn_free(err);
      destroy_session();
    }
  }

  /* 6. history round-trip + search */
  TEST("history start/end/search round-trip");
  {
    if (create_session() == 0) {
      char *id = NULL;
      if (start_command("echo smoke-test-command", &id) == 0 && id) {
        char *out = NULL;
        if (end_command_sync(id) == 0 &&
            prefix_search("echo smoke-test", 10, &out) == 0 && out &&
            strstr(out, "echo smoke-test-command") != NULL) {
          PASS();
        } else {
          FAIL("round-trip produced unexpected result");
        }
        fn_free(out);
        fn_free(id);
      } else {
        FAIL("history_start failed");
      }
      destroy_session();
    }
  }

  /* 6b. generic search options use the upstream search path */
  TEST("generic atuin_search options");
  {
    if (create_session() == 0) {
      char *id = NULL;
      if (start_command("echo generic-smoke-test", &id) == 0 && id) {
        const char *all_user = "$all-user";
        const char *authors[] = { all_user };
        atuin_search_options_t opts;
        char *out = NULL;
        memset(&opts, 0, sizeof(opts));
        opts.query = "echo generic";
        opts.search_mode = ATUIN_SEARCH_MODE_PREFIX;
        opts.has_limit = 1;
        opts.limit = 10;
        opts.authors = authors;
        opts.author_count = 1;

        end_command_sync(id);
        char *err = fn_search(&opts, &out);
        if (!err && out && strstr(out, "echo generic-smoke-test") != NULL) {
          PASS();
        } else {
          char msg[512];
          take_error(err, msg, sizeof(msg));
          printf("  (search error: %s)\n", msg);
          FAIL("generic search did not find command");
        }
        fn_free(out);
        fn_free(id);
      } else {
        FAIL("history_start failed");
      }
      destroy_session();
    }
  }

  /* 6c. repeatable --exit / --exclude-exit filters */
  TEST("exit filters are repeatable");
  {
    if (create_session() == 0) {
      char *id = NULL;
      start_command("echo exit-filter-ok", &id);
      end_command_exit(id, 0);
      fn_free(id);

      id = NULL;
      start_command("echo exit-filter-one", &id);
      end_command_exit(id, 1);
      fn_free(id);

      id = NULL;
      start_command("echo exit-filter-two", &id);
      end_command_exit(id, 2);
      fn_free(id);

      const int64_t include[] = { 1, 2 };
      char *out = NULL;
      int include_ok =
          search_with_exits("echo exit-filter", include, 2, NULL, 0, &out) == 0 &&
          out && strstr(out, "echo exit-filter-one") &&
          strstr(out, "echo exit-filter-two") &&
          strstr(out, "echo exit-filter-ok") == NULL;
      fn_free(out);

      const int64_t exclude[] = { 1 };
      out = NULL;
      int exclude_ok =
          search_with_exits("echo exit-filter", NULL, 0, exclude, 1, &out) == 0 &&
          out && strstr(out, "echo exit-filter-ok") &&
          strstr(out, "echo exit-filter-two") &&
          strstr(out, "echo exit-filter-one") == NULL;
      fn_free(out);

      if (include_ok && exclude_ok)
        PASS();
      else
        FAIL("repeatable exit filters produced unexpected results");
      destroy_session();
    }
  }

  /* 6d. atuin_history_start forwards --author-kind */
  TEST("history start author_kind");
  {
    if (create_session() == 0) {
      char *id = NULL;
      int accepted = start_command_full("echo author-kind-agent", "claude",
                                        "agent", "why", &id) == 0 &&
                     id != NULL;
      fn_free(id);

      char *bad_id = (char *)0x1;
      char *err =
          fn_history_start("echo bad-kind", "/tmp", "claude", "robot", NULL,
                           &bad_id);
      int rejected = err != NULL && bad_id == NULL;
      fn_free(err);

      if (accepted && rejected)
        PASS();
      else
        FAIL("author_kind was not forwarded or validated");
      destroy_session();
    }
  }

  /* 7. async end followed by sync end is safe */
  TEST("async then sync end");
  {
    if (create_session() == 0) {
      char *id = NULL;
      start_command("echo async-smoke-test", &id);
      char *async_err = fn_history_end(id, 0, 0, 0);
      char *sync_err = fn_history_end(id, 0, 0, 1);
      if (id && !async_err && !sync_err)
        PASS();
      else
        FAIL("history_end failed");
      fn_free(async_err);
      fn_free(sync_err);
      fn_free(id);
      destroy_session();
    }
  }

  /* 8. interactive search argument validation must happen BEFORE any terminal
   * I/O (with a valid out pointer this would open the TUI and block). */
  TEST("search_interactive NULL-safety");
  {
    char *err = fn_search_interactive(NULL, 0, ATUIN_KEYMAP_MODE_AUTO, NULL);
    if (err) {
      fn_free(err);
      PASS();
    } else {
      FAIL("null out was not rejected before the TUI started");
    }
  }

  /* 9. NULL arguments are rejected and output slots reset */
  TEST("NULL argument handling");
  {
    if (create_session() == 0) {
      char *out = (char *)0x1; /* poison; must be reset to NULL */
      char *err = fn_history_start(NULL, NULL, NULL, NULL, NULL, &out);
      if (err && out == NULL)
        PASS();
      else
        FAIL("history_start did not reject NULL / reset output");
      fn_free(err);

      out = (char *)0x1;
      err = fn_search(NULL, &out);
      if (err && out == NULL)
        PASS();
      else
        FAIL("search did not reject NULL options / reset output");
      fn_free(err);
      destroy_session();
    }
  }

  /* 10. errors are call-local: a failed call returns its own error string and
   * leaves the active session usable. */
  TEST("errors do not poison session");
  {
    if (create_session() == 0) {
      char *err = fn_history_start(NULL, NULL, NULL, NULL, NULL, NULL);
      int failed_as_expected = (err != NULL);
      fn_free(err);

      char *id = NULL;
      int still_ok = (start_command("echo after-error", &id) == 0);
      fn_free(id);

      if (failed_as_expected && still_ok)
        PASS();
      else
        FAIL("a failed call poisoned the session");
      destroy_session();
    }
  }

  /* 10b. stats reflect the session's operations */
  TEST("session stats");
  {
    if (create_session() == 0) {
      atuin_stats_t st;
      memset(&st, 0, sizeof(st));
      char *err = fn_stats(&st);
      int started_at_zero = (!err && st.history_starts == 0);

      char *id = NULL;
      start_command("echo stats-smoke-test", &id);
      end_command_sync(id);
      fn_free(id);

      char *out = NULL;
      (void)prefix_search("echo stats", 5, &out);
      fn_free(out);

      memset(&st, 0, sizeof(st));
      err = fn_stats(&st);
      if (err) {
        fn_free(err);
        FAIL("atuin_stats failed");
      } else if (started_at_zero && st.history_starts == 1 &&
                 st.history_ends_sync == 1 && st.search_prefix_calls == 1 &&
                 st.search_calls >= 1) {
        PASS();
      } else {
        FAIL("stats did not count the session's operations");
      }
      destroy_session();
    }
  }

  /* 10c. stats without a session are rejected and zeroed */
  TEST("stats without session rejected");
  {
    atuin_stats_t st;
    memset(&st, 0xff, sizeof(st));
    char *err = fn_stats(&st);
    if (err && st.history_starts == 0) {
      fn_free(err);
      PASS();
    } else {
      fn_free(err);
      FAIL("stats without a session was not rejected/zeroed");
    }
  }

  /* 10d. destroy then create resets the counters */
  TEST("destroy then create resets stats");
  {
    if (create_session() == 0) {
      char *id = NULL;
      start_command("echo stats-reset-test", &id);
      fn_free(id);
      destroy_session();
    }

    if (create_session() == 0) {
      atuin_stats_t st;
      memset(&st, 0, sizeof(st));
      char *err = fn_stats(&st);
      if (!err && st.history_starts == 0 && st.search_calls == 0) {
        PASS();
      } else {
        fn_free(err);
        FAIL("re-created session did not reset its stats");
      }
      destroy_session();
    }
  }

  /* 11. create() follows the settings data directory (env / config.toml) */
  TEST("create() uses settings data dir");
  {
    char override_dir[128];
    snprintf(override_dir, sizeof(override_dir), "%s/override", data_dir);
    setenv("ATUIN_DATA_DIR", override_dir, 1);

    if (create_session() == 0) {
      char *id = NULL;
      if (start_command("echo default-data-dir-test", &id) == 0 && id) {
        end_command_sync(id);
        fn_free(id);
      }
      destroy_session();

      char history_path[sizeof(override_dir) + 32];
      snprintf(history_path, sizeof(history_path), "%s/history.db", override_dir);
      /* The settings path caches are cleared on destroy, so the re-created
       * session must also put meta.db under the newly resolved data dir. If
       * they were stale, meta.db would stay in the previous directory. */
      char meta_path[sizeof(override_dir) + 32];
      snprintf(meta_path, sizeof(meta_path), "%s/meta.db", override_dir);
      if (access(history_path, F_OK) == 0 && access(meta_path, F_OK) == 0)
        PASS();
      else
        FAIL("settings data dir history.db/meta.db was not created");
    } else {
      FAIL("create() failed");
    }

    /* Restore the shared test data dir for the remaining checks. */
    setenv("ATUIN_DATA_DIR", atuin_data, 1);
  }

  /* 12. fork guard: a child inherited via fork() must be refused */
  TEST("fork guard rejects child");
  {
    if (create_session() == 0) {
      pid_t pid = fork();
      if (pid == 0) {
        char *out = NULL;
        char *err =
            fn_history_start("echo forked-child", "/tmp", NULL, NULL, NULL, &out);
        int rejected = (err != NULL);
        fn_free(err);
        fn_free(out);
        _exit(rejected ? 0 : 1);
      }
      int status = 0;
      if (waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
          WEXITSTATUS(status) == 0) {
        PASS();
      } else {
        FAIL("fork guard did not reject the forked child");
      }
      destroy_session();
    }
  }

  printf("\nResults: %d passed, %d failed\n", passed, failed);
  if (dlclose(lib_handle) != 0) {
    fprintf(stderr, "dlclose: %s\n", dlerror());
    failed++;
  }

  /* Best-effort cleanup of the isolated data/config tree. */
  {
    const char *files[] = {
        "atuin/history.db",      "atuin/history.db-shm", "atuin/history.db-wal",
        "atuin/records.db",      "atuin/records.db-shm", "atuin/records.db-wal",
        "atuin/key",             "atuin/meta.db",        "atuin/meta.db-shm",
        "atuin/meta.db-wal",     "override/history.db",  "override/history.db-shm",
        "override/history.db-wal", "override/records.db", "override/records.db-shm",
        "override/records.db-wal", "override/key",        "override/meta.db",
        "override/meta.db-shm",   "override/meta.db-wal", "config/config.toml",
        NULL,
    };
    for (int i = 0; files[i] != NULL; i++) {
      char path[512];
      snprintf(path, sizeof(path), "%s/%s", data_dir, files[i]);
      remove(path);
    }
    {
      char subdir[160];
      const char *dirs[] = {"atuin", "override", "config"};
      for (size_t i = 0; i < sizeof(dirs) / sizeof(dirs[0]); i++) {
        snprintf(subdir, sizeof(subdir), "%s/%s", data_dir, dirs[i]);
        rmdir(subdir);
      }
    }
    rmdir(data_dir);
  }
  return failed > 0 ? 1 : 0;
}
