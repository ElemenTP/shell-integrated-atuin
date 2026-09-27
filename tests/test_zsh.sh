#!/usr/bin/env zsh
# zsh integration test for the atuin_native module.
#
# Usage:
#   MODULE_DIR=/path/to/zsh_src/build zsh tests/test_zsh.sh

set -euo pipefail

MODULE_DIR="${MODULE_DIR:-$PWD/zsh_src/build}"
ATUIN_DATA_DIR="${ATUIN_DATA_DIR:-/tmp/atuin-zsh-test-$$}"

echo "=== atuin zsh integration test ==="
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
if ! zmodload atuin_native 2>/dev/null; then
    echo "FAIL: could not load atuin_native from $MODULE_DIR"
    echo "  Available modules in path:"
    for d in $module_path; do
        ls -la "$d"/atuin_native* 2>/dev/null || true
    done
    exit 1
fi
echo "PASS: module loaded"

# history_start reads command/cwd from parameters and writes the ID to $ATUIN_HISTORY_ID.
# Command substitution is intentionally NOT used: $() forks and the FFI fork
# guard rejects in-process calls in forked children.
ATUIN_HISTORY_ID=""
ATUIN_HISTORY_COMMAND="echo hello zsh"
ATUIN_HISTORY_CWD="/tmp"
atuin_history_start >/dev/null 2>&1
id="${ATUIN_HISTORY_ID:-}"
if [[ -n "$id" ]]; then
    echo "PASS: history_start ($id)"
else
    echo "FAIL: history_start produced no id"
    exit 1
fi

ATUIN_HISTORY_ID="$id"
ATUIN_HISTORY_EXIT=0
ATUIN_HISTORY_DURATION_NS=1000000
ATUIN_HISTORY_SYNC=1
atuin_history_end
echo "PASS: history_end (sync)"

# Async end must not block or fail the prompt.
ATUIN_HISTORY_ID=""
ATUIN_HISTORY_COMMAND="echo async zsh"
ATUIN_HISTORY_CWD="/tmp"
atuin_history_start >/dev/null 2>&1
id2="${ATUIN_HISTORY_ID:-}"
[[ -n "$id2" ]] || { echo "FAIL: second history_start"; exit 1; }
ATUIN_HISTORY_ID="$id2"
ATUIN_HISTORY_EXIT=0
ATUIN_HISTORY_DURATION_NS=0
ATUIN_HISTORY_SYNC=0
atuin_history_end
echo "PASS: history_end (async)"

ATUIN_SEARCH_RESULT=""
ATUIN_SEARCH_QUERY="echo hello zsh"
ATUIN_SEARCH_LIMIT=5
atuin_search >/dev/null
if [[ "${ATUIN_SEARCH_RESULT:-}" == *"echo hello zsh"* ]]; then
    echo "PASS: search"
else
    echo "FAIL: search did not find the recorded command"
    exit 1
fi

ATUIN_SESSION=""
atuin_session_id >/dev/null
if [[ -n "${ATUIN_SESSION:-}" ]]; then
    echo "PASS: session_id"
else
    echo "FAIL: session_id did not set ATUIN_SESSION"
    exit 1
fi

ATUIN_VERSION=""
atuin_version >/dev/null
if [[ -n "${ATUIN_VERSION:-}" ]]; then
    echo "PASS: version (${ATUIN_VERSION})"
else
    echo "FAIL: version did not set ATUIN_VERSION"
    exit 1
fi

# Generic search accepts upstream-compatible options.
ATUIN_SEARCH_RESULT=""
ATUIN_SEARCH_QUERY="echo hello zsh"
ATUIN_SEARCH_MODE="prefix"
ATUIN_SEARCH_LIMIT=5
ATUIN_SEARCH_AUTHORS=('$all-user')
atuin_search >/dev/null
if [[ "${ATUIN_SEARCH_RESULT:-}" == *"echo hello zsh"* ]]; then
    echo "PASS: search with upstream-compatible options"
else
    echo "FAIL: generic search did not find the recorded command"
    exit 1
fi

# history_start forwards $ATUIN_HISTORY_AUTHOR_KIND.
ATUIN_HISTORY_ID=""
ATUIN_HISTORY_COMMAND="echo agent-kind zsh"
ATUIN_HISTORY_CWD="/tmp"
ATUIN_HISTORY_AUTHOR="claude"
ATUIN_HISTORY_AUTHOR_KIND="agent"
atuin_history_start >/dev/null
agent_id="${ATUIN_HISTORY_ID:-}"
if [[ -n "$agent_id" ]]; then
    ATUIN_HISTORY_ID="$agent_id"
    ATUIN_HISTORY_EXIT=0
    ATUIN_HISTORY_DURATION_NS=0
    ATUIN_HISTORY_SYNC=1
    atuin_history_end >/dev/null
    unset ATUIN_HISTORY_AUTHOR ATUIN_HISTORY_AUTHOR_KIND
    ATUIN_SEARCH_RESULT=""
    ATUIN_SEARCH_QUERY="echo agent-kind zsh"
    ATUIN_SEARCH_MODE="prefix"
    ATUIN_SEARCH_AUTHORS=('$all-agent')
    atuin_search >/dev/null
    unset ATUIN_SEARCH_AUTHORS
    if [[ "${ATUIN_SEARCH_RESULT:-}" == *"echo agent-kind zsh"* ]]; then
        echo "PASS: ATUIN_HISTORY_AUTHOR_KIND is recorded"
    else
        echo "FAIL: stated agent author_kind was not recorded"
        exit 1
    fi
else
    echo "FAIL: history_start with author_kind produced no ID"
    exit 1
fi

# Repeatable --exit / --exclude-exit via $ATUIN_SEARCH_EXITS arrays.
ATUIN_HISTORY_ID=""
ATUIN_HISTORY_COMMAND="echo exit-filter zsh"
ATUIN_HISTORY_CWD="/tmp"
atuin_history_start >/dev/null
exit_id="${ATUIN_HISTORY_ID:-}"
if [[ -n "$exit_id" ]]; then
    ATUIN_HISTORY_ID="$exit_id"
    ATUIN_HISTORY_EXIT=7
    ATUIN_HISTORY_DURATION_NS=0
    ATUIN_HISTORY_SYNC=1
    atuin_history_end >/dev/null

    ATUIN_SEARCH_RESULT=""
    ATUIN_SEARCH_QUERY="echo exit-filter zsh"
    ATUIN_SEARCH_MODE="prefix"
    ATUIN_SEARCH_EXITS=(7 130)
    atuin_search >/dev/null
    unset ATUIN_SEARCH_EXITS
    if [[ "${ATUIN_SEARCH_RESULT:-}" == *"echo exit-filter zsh"* ]]; then
        echo "PASS: ATUIN_SEARCH_EXITS includes a matching code"
    else
        echo "FAIL: ATUIN_SEARCH_EXITS did not match exit 7"
        exit 1
    fi

    ATUIN_SEARCH_RESULT=""
    ATUIN_SEARCH_EXCLUDE_EXITS=(7)
    atuin_search >/dev/null
    unset ATUIN_SEARCH_EXCLUDE_EXITS
    if [[ "${ATUIN_SEARCH_RESULT:-}" != *"echo exit-filter zsh"* ]]; then
        echo "PASS: ATUIN_SEARCH_EXCLUDE_EXITS drops a matching code"
    else
        echo "FAIL: ATUIN_SEARCH_EXCLUDE_EXITS kept exit 7"
        exit 1
    fi
else
    echo "FAIL: history_start for the exit-filter test produced no ID"
    exit 1
fi

# Dedicated autosuggest fast path (Session::search_prefix).
ATUIN_SEARCH_RESULT=""
ATUIN_SEARCH_QUERY="echo hello zsh"
ATUIN_SEARCH_LIMIT=1
atuin_search_prefix >/dev/null
if [[ "${ATUIN_SEARCH_RESULT:-}" == "echo hello zsh" ]]; then
    echo "PASS: search_prefix fast path"
else
    echo "FAIL: search_prefix did not return the newest match: ${ATUIN_SEARCH_RESULT:-}"
    exit 1
fi

# Session statistics.
ATUIN_STATS_QUIET=1
atuin_stats >/dev/null
if [[ "${ATUIN_STATS_HISTORY_STARTS:-0}" -ge 1 &&
      "${ATUIN_STATS_SEARCH_PREFIX_CALLS:-0}" -ge 1 &&
      "${ATUIN_STATS_SEARCH_CALLS:-0}" -ge 1 ]]; then
    echo "PASS: stats (starts=${ATUIN_STATS_HISTORY_STARTS}, prefix=${ATUIN_STATS_SEARCH_PREFIX_CALLS})"
else
    echo "FAIL: stats counters were not set"
    exit 1
fi

# The optional limit parser rejects malformed input.
ATUIN_SEARCH_QUERY="echo"
ATUIN_SEARCH_LIMIT="not-a-number"
if atuin_search 2>/dev/null; then
    echo "FAIL: invalid search limit was accepted"
    exit 1
else
    echo "PASS: invalid search limit rejected"
fi

if zmodload -u atuin_native 2>/dev/null; then
    echo "PASS: module unloaded"
else
    echo "FAIL: module unload failed"
    exit 1
fi

rm -rf "$ATUIN_DATA_DIR"
echo "=== All zsh integration tests passed ==="
