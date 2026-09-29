#!/usr/bin/env zsh
# Fork safety regression test for the atuin_native zsh module.
#
# Verifies that builtins called from forked contexts do not crash. The fork
# guard in the FFI layer must reject calls in forked children.
#
# Usage:
#   MODULE_DIR=/path/to/zsh_src/build zsh tests/test_fork_zsh.sh

set -euo pipefail

MODULE_DIR="${MODULE_DIR:-$PWD/zsh_src/build}"
ATUIN_DATA_DIR="${ATUIN_DATA_DIR:-/tmp/atuin-fork-test-$$}"

echo "=== atuin fork safety test ==="
echo "Module dir: $MODULE_DIR"

mkdir -p "$ATUIN_DATA_DIR"
export ATUIN_DATA_DIR

# atuin_init takes no data-dir argument any more: the session
# resolves its paths from the settings tree. Pin data_dir in an isolated
# config.toml so the tests never touch the real ~/.config/atuin.
export ATUIN_CONFIG_DIR="$ATUIN_DATA_DIR/config"
mkdir -p "$ATUIN_CONFIG_DIR"
printf 'data_dir = "%s"\n' "$ATUIN_DATA_DIR" > "$ATUIN_CONFIG_DIR/config.toml"

module_path=("$MODULE_DIR" $module_path)
zmodload atuin_native || { echo "FAIL: load"; exit 1; }
echo "PASS: load"

# Sanity: direct call in the parent process.
ATUIN_HISTORY_ID=""
ATUIN_HISTORY_COMMAND="echo direct"
ATUIN_HISTORY_CWD="/tmp"
atuin_history_start >/dev/null 2>&1
id="${ATUIN_HISTORY_ID:-}"
[[ -n "$id" ]] && echo "PASS: history_start in parent" || { echo "FAIL: start"; exit 1; }

# ---- Forked contexts (the guard must actually reject, not merely survive) ----
#
# zsh forks for $(), &, non-final pipeline stages, subshells and process
# substitution. In each child the FFI fork guard must refuse the call with
# "refusing call in forked child process" on stderr; touching the inherited
# tokio runtime would corrupt it.
errfile="$ATUIN_DATA_DIR/fork.err"
: > "$errfile"

expect_fork_rejected() {
    local label="$1" tries=0
    while (( tries < 100 )); do
        if grep -q "forked child process" "$errfile" 2>/dev/null; then
            break
        fi
        sleep 0.02
        (( tries++ )) || true
    done
    if ! grep -q "forked child process" "$errfile" 2>/dev/null; then
        echo "FAIL: $label was not rejected by the fork guard"
        echo "      stderr: $(cat "$errfile" 2>/dev/null)"
        exit 1
    fi
    echo "PASS: $label rejected by fork guard"
    : > "$errfile"
}

# $() command substitution
ATUIN_HISTORY_COMMAND="echo subshell"
ATUIN_HISTORY_CWD="/tmp"
: > "$errfile"
sub_id=$(atuin_history_start 2>"$errfile") || true
expect_fork_rejected '$() command substitution'

# Background job &
: > "$errfile"
(ATUIN_HISTORY_COMMAND="echo background"; ATUIN_HISTORY_CWD="/tmp"; \
    atuin_history_start 2>"$errfile") &
bg_pid=$!
wait $bg_pid 2>/dev/null || true
expect_fork_rejected 'background &'

# Pipeline, builtin not the last stage
ATUIN_HISTORY_COMMAND="echo pipeline"
ATUIN_HISTORY_CWD="/tmp"
: > "$errfile"
atuin_history_start 2>"$errfile" | cat >/dev/null || true
expect_fork_rejected 'pipeline (builtin not last stage)'

# Subshell ( )
: > "$errfile"
(ATUIN_HISTORY_COMMAND="echo subshell2"; ATUIN_HISTORY_CWD="/tmp"; \
    atuin_history_start 2>"$errfile") || true
expect_fork_rejected 'subshell ( )'

# Process substitution <( )
: > "$errfile"
cat <(ATUIN_HISTORY_COMMAND="echo procsubst"; ATUIN_HISTORY_CWD="/tmp"; \
    atuin_history_start 2>"$errfile") >/dev/null 2>&1 || true
expect_fork_rejected 'process substitution <( )'

# Nested command substitution
: > "$errfile"
outer=$(echo $(ATUIN_HISTORY_COMMAND="echo nested"; ATUIN_HISTORY_CWD="/tmp"; \
    atuin_history_start 2>"$errfile")) || true
expect_fork_rejected 'nested $()'

# ---- Parent still works after forks ----
ATUIN_HISTORY_ID=""
ATUIN_HISTORY_COMMAND="echo after-forks"
ATUIN_HISTORY_CWD="/tmp"
atuin_history_start >/dev/null 2>&1
id2="${ATUIN_HISTORY_ID:-}"
[[ -n "$id2" ]] && echo "PASS: parent functional after forks" || { echo "FAIL: parent broken"; exit 1; }

ATUIN_HISTORY_ID="$id2"
ATUIN_HISTORY_EXIT=0
ATUIN_HISTORY_DURATION_NS=0
ATUIN_HISTORY_SYNC=1
atuin_history_end 2>/dev/null
echo "PASS: history_end in parent"

# ---- Unload/reload ----
zmodload -u atuin_native
echo "PASS: unload"
zmodload atuin_native
ATUIN_HISTORY_ID=""
ATUIN_HISTORY_COMMAND="echo reloaded"
ATUIN_HISTORY_CWD="/tmp"
atuin_history_start >/dev/null 2>&1
id3="${ATUIN_HISTORY_ID:-}"
[[ -n "$id3" ]] && echo "PASS: reload + start" || { echo "FAIL: reload"; exit 1; }
zmodload -u atuin_native
echo "PASS: final unload"

rm -rf "$ATUIN_DATA_DIR"
echo "=== All atuin fork tests passed ==="
