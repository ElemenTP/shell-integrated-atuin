/*
 * test_zsh_module_unit.c — unit tests for the pure zsh builtin helpers.
 *
 * atuin_module.c itself depends on zsh internal headers and cannot be linked
 * with an ordinary C compiler. The argument-parsing logic lives in
 * zsh_src/atuin_builtin_util.{h,c}, which has zero zsh dependencies and is
 * exercised here.
 *
 * Build:
 *   cc -std=c11 -Wall -Wextra -Werror -o test_zsh_module_unit \
 *       tests/test_zsh_module_unit.c zsh_src/atuin_builtin_util.c
 */

#include "atuin_builtin_util.h"

#include <stdio.h>
#include <string.h>

static int g_passed = 0;
static int g_failed = 0;

#define CHECK(cond)                                                            \
    do {                                                                       \
        if (cond) {                                                            \
            g_passed++;                                                        \
        } else {                                                               \
            g_failed++;                                                        \
            fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);    \
        }                                                                      \
    } while (0)

#define CHECK_EQ_INT(a, b)                                                     \
    do {                                                                       \
        long _a = (long)(a);                                                   \
        long _b = (long)(b);                                                   \
        if (_a == _b) {                                                        \
            g_passed++;                                                        \
        } else {                                                               \
            g_failed++;                                                        \
            fprintf(stderr, "FAIL %s:%d: %s == %s (got %ld, want %ld)\n",      \
                    __FILE__, __LINE__, #a, #b, _a, _b);                       \
        }                                                                      \
    } while (0)

static void test_parse_long(void) {
    long out = -1;

    CHECK(atuin_parse_long("0", &out) == 0);
    CHECK_EQ_INT(out, 0);

    CHECK(atuin_parse_long("123456789", &out) == 0);
    CHECK_EQ_INT(out, 123456789);

    CHECK(atuin_parse_long("-42", &out) == 0);
    CHECK_EQ_INT(out, -42);

    CHECK(atuin_parse_long(" 12", &out) != 0);
    CHECK(atuin_parse_long("12 ", &out) != 0);
    CHECK(atuin_parse_long("", &out) != 0);
    CHECK(atuin_parse_long(NULL, &out) != 0);
    CHECK(atuin_parse_long("12x", &out) != 0);
    CHECK(atuin_parse_long("999999999999999999999999", &out) != 0);
}

static void test_parse_end_options(void) {
    atuin_end_options_t opts;

    /* No optional arguments. */
    char *empty[] = {NULL};
    CHECK(atuin_parse_end_options(empty, &opts) == 0);
    CHECK_EQ_INT(opts.duration_ns, 0);
    CHECK_EQ_INT(opts.sync, 0);

    /* Positional duration. */
    char *positional[] = {"12345", NULL};
    CHECK(atuin_parse_end_options(positional, &opts) == 0);
    CHECK_EQ_INT(opts.duration_ns, 12345);
    CHECK_EQ_INT(opts.sync, 0);

    /* --sync only. */
    char *sync_only[] = {"--sync", NULL};
    CHECK(atuin_parse_end_options(sync_only, &opts) == 0);
    CHECK_EQ_INT(opts.duration_ns, 0);
    CHECK_EQ_INT(opts.sync, 1);

    /* Duration and --sync in either order. */
    char *both[] = {"--sync", "--duration=987", NULL};
    CHECK(atuin_parse_end_options(both, &opts) == 0);
    CHECK_EQ_INT(opts.duration_ns, 987);
    CHECK_EQ_INT(opts.sync, 1);

    char *both_reversed[] = {"987", "--sync", NULL};
    CHECK(atuin_parse_end_options(both_reversed, &opts) == 0);
    CHECK_EQ_INT(opts.duration_ns, 987);
    CHECK_EQ_INT(opts.sync, 1);

    /* Malformed input. */
    char *bad[] = {"abc", NULL};
    CHECK(atuin_parse_end_options(bad, &opts) != 0);
    char *bad_flag[] = {"--duration=", NULL};
    CHECK(atuin_parse_end_options(bad_flag, &opts) != 0);
    char *negative[] = {"-1", NULL};
    CHECK(atuin_parse_end_options(negative, &opts) != 0);
    char *unknown_flag[] = {"--wat", NULL};
    CHECK(atuin_parse_end_options(unknown_flag, &opts) != 0);

    CHECK(atuin_parse_end_options(NULL, &opts) == 0);
    CHECK_EQ_INT(opts.duration_ns, 0);
    CHECK_EQ_INT(opts.sync, 0);
    CHECK(atuin_parse_end_options(empty, NULL) != 0);
}

static void test_parse_limit(void) {
    CHECK_EQ_INT(atuin_parse_limit(NULL, 10, 1000), 10);
    CHECK_EQ_INT(atuin_parse_limit("1", 10, 1000), 1);
    CHECK_EQ_INT(atuin_parse_limit("0", 10, 1000), 0);
    CHECK_EQ_INT(atuin_parse_limit("999999", 10, 1000), 1000);
    CHECK_EQ_INT(atuin_parse_limit("-1", 10, 1000), -1);
    CHECK_EQ_INT(atuin_parse_limit("abc", 10, 1000), -1);
    CHECK_EQ_INT(atuin_parse_limit("", 10, 1000), -1);
}

int main(void) {
    printf("zsh module helper unit tests\n");

    test_parse_long();
    test_parse_end_options();
    test_parse_limit();

    printf("results: %d passed, %d failed\n", g_passed, g_failed);
    return g_failed > 0 ? 1 : 0;
}
