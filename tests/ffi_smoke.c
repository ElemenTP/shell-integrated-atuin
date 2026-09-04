#define _GNU_SOURCE

/*
 * ffi_smoke.c — dlopen-based smoke/system test for the atuin-ffi C API.
 *
 * It loads libatuin_ffi.so at runtime exactly like zsh's zmodload would, then
 * exercises every exported function: lifecycle, history round-trip, search,
 * metadata, error handling, NULL safety, and the fork guard.
 *
 * Build:
 *   cc -std=c11 -Wall -Wextra -Werror -o ffi_smoke ffi_smoke.c -ldl
 *
 * Usage:
 *   ./ffi_smoke [path/to/libatuin_ffi.so]
 */

#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

typedef struct atuin_session atuin_session_t;

static void *lib_handle = NULL;
static atuin_session_t *(*fn_session_create)(const char *);
static void (*fn_session_destroy)(atuin_session_t *);
static int (*fn_history_start)(atuin_session_t *, const char *, const char *,
                               const char *, const char *, char **);
static int (*fn_history_end)(atuin_session_t *, const char *, int64_t, int64_t,
                             int);
static int (*fn_search_prefix)(atuin_session_t *, const char *, int, char **);
static void (*fn_free_string)(char *);
static const char *(*fn_session_uuid)(atuin_session_t *);
static const char *(*fn_version)(void);
static const char *(*fn_last_error)(void);

static int passed = 0, failed = 0;
static char data_dir[] = "/tmp/atuin-ffi-smoke-XXXXXX";

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
#define L(sym)                                                                 \
  *(void **)(&fn_##sym) = dlsym(lib_handle, "atuin_" #sym);                    \
  if (!fn_##sym) {                                                             \
    fprintf(stderr, "dlsym(atuin_%s): %s\n", #sym, dlerror());                 \
    return -1;                                                                 \
  }
  L(session_create);
  L(session_destroy);
  L(history_start);
  L(history_end);
  L(search_prefix);
  L(free_string);
  L(session_uuid);
  L(version);
  L(last_error);
#undef L
  return 0;
}

static atuin_session_t *create_session(void) {
  atuin_session_t *s = fn_session_create(data_dir);
  if (!s) {
    FAIL("session_create failed");
    return NULL;
  }
  return s;
}

int main(int argc, char **argv) {
  const char *libpath =
      (argc > 1) ? argv[1] : "rust_src/target/release/libatuin_ffi.so";
  printf("Atuin FFI Smoke Test\nLibrary: %s\n\n", libpath);

  if (mkdtemp(data_dir) == NULL) {
    perror("mkdtemp");
    return 1;
  }
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
    atuin_session_t *s = create_session();
    if (s) {
      fn_session_destroy(s);
      PASS();
    }
  }

  /* 3. destroy(NULL) */
  TEST("destroy(NULL)");
  {
    fn_session_destroy(NULL);
    PASS();
  }

  /* 4. free_string(NULL) */
  TEST("free_string(NULL)");
  {
    fn_free_string(NULL);
    PASS();
  }

  /* 5. session UUID */
  TEST("session UUID stable");
  {
    atuin_session_t *s = create_session();
    if (s) {
      const char *a = fn_session_uuid(s);
      const char *b = fn_session_uuid(s);
      if (a && b && strcmp(a, b) == 0)
        PASS();
      else
        FAIL("UUID changed or was null");
      fn_session_destroy(s);
    }
  }

  /* 6. history round-trip + search */
  TEST("history start/end/search round-trip");
  {
    atuin_session_t *s = create_session();
    if (s) {
      char *id = NULL;
      int rc = fn_history_start(s, "echo smoke-test-command", "/tmp", NULL,
                                NULL, &id);
      if (rc == 0 && id && strlen(id) > 0) {
        int end_rc = fn_history_end(s, id, 0, 123456, 1);
        char *out = NULL;
        int search_rc = fn_search_prefix(s, "echo smoke-test", 10, &out);
        if (end_rc == 0 && search_rc == 0 && out &&
            strstr(out, "echo smoke-test-command") != NULL) {
          PASS();
        } else {
          FAIL("round-trip produced unexpected result");
        }
        fn_free_string(out);
        fn_free_string(id);
      } else {
        FAIL("history_start failed");
      }
      fn_session_destroy(s);
    }
  }

  /* 7. async end followed by sync end is safe */
  TEST("async then sync end");
  {
    atuin_session_t *s = create_session();
    if (s) {
      char *id = NULL;
      fn_history_start(s, "echo async-smoke-test", "/tmp", NULL, NULL, &id);
      int async_rc = fn_history_end(s, id, 0, 0, 0);
      int sync_rc = fn_history_end(s, id, 0, 0, 1);
      if (id && async_rc == 0 && sync_rc == 0)
        PASS();
      else
        FAIL("history_end failed");
      fn_free_string(id);
      fn_session_destroy(s);
    }
  }

  /* 8. NULL arguments are rejected and output slots reset */
  TEST("NULL argument handling");
  {
    atuin_session_t *s = create_session();
    if (s) {
      char *out = (char *)0x1; /* poison; must be reset to NULL */
      int rc = fn_history_start(s, NULL, NULL, NULL, NULL, &out);
      if (rc < 0 && out == NULL)
        PASS();
      else
        FAIL("history_start did not reject NULL / reset output");

      out = (char *)0x1;
      rc = fn_search_prefix(NULL, NULL, 1, &out);
      if (rc < 0 && out == NULL)
        PASS();
      else
        FAIL("search did not reject NULL / reset output");
      fn_session_destroy(s);
    }
  }

  /* 9. last-error lifecycle */
  TEST("last-error lifecycle");
  {
    atuin_session_t *s = create_session();
    if (s) {
      char *out = NULL;
      fn_history_start(NULL, NULL, NULL, NULL, NULL, &out);
      const char *err = fn_last_error();
      if (err && strlen(err) > 0) {
        char *id = NULL;
        fn_history_start(s, "echo clear-error-smoke", "/tmp", NULL, NULL, &id);
        fn_free_string(id);
        if (fn_last_error() == NULL)
          PASS();
        else
          FAIL("error was not cleared by successful call");
      } else {
        FAIL("expected an error message");
      }
      fn_session_destroy(s);
    }
  }

  /* 10. create(NULL) resolves the default data dir from settings/env */
  TEST("create(NULL) uses settings data dir");
  {
    char atuin_subdir[sizeof(data_dir) + 32];
    snprintf(atuin_subdir, sizeof(atuin_subdir), "%s/atuin", data_dir);
    setenv("XDG_DATA_HOME", data_dir, 1);

    atuin_session_t *s = fn_session_create(NULL);
    if (s) {
      char *id = NULL;
      int rc = fn_history_start(s, "echo default-data-dir-test", "/tmp", NULL,
                                NULL, &id);
      if (rc == 0 && id) {
        fn_history_end(s, id, 0, 0, 1);
        fn_free_string(id);
      }
      fn_session_destroy(s);

      char history_path[sizeof(data_dir) + 64];
      snprintf(history_path, sizeof(history_path), "%s/history.db", atuin_subdir);
      if (access(history_path, F_OK) == 0)
        PASS();
      else
        FAIL("default-data-dir history.db was not created");
    } else {
      FAIL("create(NULL) failed");
    }

    unsetenv("XDG_DATA_HOME");
  }

  /* 11. fork guard: a child inherited via fork() must be refused */
  TEST("fork guard rejects child");
  {
    atuin_session_t *s = create_session();
    if (s) {
      pid_t pid = fork();
      if (pid == 0) {
        char *out = NULL;
        int rc = fn_history_start(s, "echo forked-child", "/tmp", NULL, NULL,
                                  &out);
        fn_free_string(out);
        _exit(rc < 0 ? 0 : 1);
      }
      int status = 0;
      if (waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
          WEXITSTATUS(status) == 0) {
        PASS();
      } else {
        FAIL("fork guard did not reject the forked child");
      }
      fn_session_destroy(s);
    }
  }

  printf("\nResults: %d passed, %d failed\n", passed, failed);
  if (dlclose(lib_handle) != 0) {
    fprintf(stderr, "dlclose: %s\n", dlerror());
    failed++;
  }

  /* Best-effort cleanup of the isolated data directory. */
  {
    const char *files[] = {
        "history.db",     "history.db-shm", "history.db-wal",
        "records.db",     "records.db-shm", "records.db-wal",
        "key",            "meta.db",        "meta.db-shm",
        "meta.db-wal",    "atuin/history.db", "atuin/history.db-shm",
        "atuin/history.db-wal", "atuin/records.db", "atuin/records.db-shm",
        "atuin/records.db-wal", "atuin/key", NULL,
    };
    for (int i = 0; files[i] != NULL; i++) {
      char path[512];
      snprintf(path, sizeof(path), "%s/%s", data_dir, files[i]);
      remove(path);
    }
    {
      char atuin_subdir[sizeof(data_dir) + 32];
      snprintf(atuin_subdir, sizeof(atuin_subdir), "%s/atuin", data_dir);
      rmdir(atuin_subdir);
    }
    rmdir(data_dir);
  }
  return failed > 0 ? 1 : 0;
}
