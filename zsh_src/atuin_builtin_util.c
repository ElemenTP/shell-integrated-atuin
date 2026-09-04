/*
 * atuin_builtin_util.c — pure parsing helpers for the atuin zsh builtins.
 *
 * Kept dependency-free so it can be unit tested with an ordinary C compiler
 * (see tests/test_zsh_module_unit.c) and reused directly by atuin_module.c.
 */
#include "atuin_builtin_util.h"

#include <ctype.h>
#include <errno.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>

int atuin_parse_long(const char *text, long *out) {
    char *end = NULL;

    if (text == NULL || *text == '\0' || isspace((unsigned char)*text)) {
        return -1;
    }

    errno = 0;
    long value = strtol(text, &end, 10);
    if (errno == ERANGE || end == text || *end != '\0') {
        return -1;
    }

    if (out != NULL) {
        *out = value;
    }
    return 0;
}

int atuin_parse_end_options(char **args, atuin_end_options_t *out) {
    if (out == NULL) {
        return -1;
    }

    atuin_end_options_t parsed = {0, 0};

    if (args == NULL) {
        if (out != NULL) {
            *out = parsed;
        }
        return 0;
    }

    for (char **arg = args; *arg != NULL; arg++) {
        if (strcmp(*arg, "--sync") == 0) {
            parsed.sync = 1;
            continue;
        }

        const char *number = *arg;
        if (strncmp(number, "--duration=", 11) == 0) {
            number += 11;
            if (*number == '\0') {
                return -1;
            }
        }

        if (atuin_parse_long(number, &parsed.duration_ns) != 0
            || parsed.duration_ns < 0) {
            return -1;
        }
    }

    *out = parsed;
    return 0;
}

int atuin_parse_limit(const char *text, int default_value, int max_value) {
    if (text == NULL) {
        return default_value;
    }

    long parsed = 0;
    if (atuin_parse_long(text, &parsed) != 0 || parsed < 0) {
        return -1;
    }

    if (max_value > 0 && parsed > (long)max_value) {
        parsed = max_value;
    }
    return (int)parsed;
}
