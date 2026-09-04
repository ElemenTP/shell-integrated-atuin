/*
 * atuin_builtin_util.h — pure helpers for the atuin zsh builtins.
 *
 * This header has no zsh headers or zsh-internal dependencies on purpose:
 * the same .c file is compiled into the loadable module AND linked into the
 * standalone C unit test in tests/test_zsh_module_unit.c.
 */
#ifndef ATUIN_BUILTIN_UTIL_H
#define ATUIN_BUILTIN_UTIL_H

#ifdef __cplusplus
extern "C" {
#endif

/* Parsed options for atuin_history_end <id> <exit> [options...]. */
typedef struct atuin_end_options {
    long duration_ns; /* 0 when not supplied */
    int sync;         /* 1 when --sync is present */
} atuin_end_options_t;

/* Strictly parse a base-10 long. Returns 0 and writes *out on success. */
int atuin_parse_long(const char *text, long *out);

/* Parse the trailing arguments of atuin_history_end.
 *
 * `args` is the NULL-terminated zsh builtin argv starting at the first
 * optional argument (after the required id and exit code).
 * Recognized forms:
 *   123456789          duration in nanoseconds
 *   --duration=12345   duration in nanoseconds
 *   --sync             wait for the asynchronous DB update
 *
 * Returns 0 on success, -1 on malformed input. */
int atuin_parse_end_options(char **args, atuin_end_options_t *out);

/* Parse the optional limit argument of atuin_search.
 * Returns default_value when text is NULL, a clamped non-negative value on
 * success, or -1 on malformed input. */
int atuin_parse_limit(const char *text, int default_value, int max_value);

#ifdef __cplusplus
}
#endif

#endif /* ATUIN_BUILTIN_UTIL_H */
