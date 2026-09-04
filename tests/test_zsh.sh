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

# history_start writes the ID both to stdout and to $ATUIN_HISTORY_ID.
# Command substitution is intentionally NOT used: $() forks and the FFI fork
# guard rejects in-process calls in forked children.
ATUIN_HISTORY_ID=""
atuin_history_start "echo hello zsh" "/tmp" >/dev/null 2>&1
id="${ATUIN_HISTORY_ID:-}"
if [[ -n "$id" ]]; then
    echo "PASS: history_start ($id)"
else
    echo "FAIL: history_start produced no id"
    exit 1
fi

atuin_history_end "$id" 0 1000000 --sync
echo "PASS: history_end (sync)"

# Async end must not block or fail the prompt.
ATUIN_HISTORY_ID=""
atuin_history_start "echo async zsh" "/tmp" >/dev/null 2>&1
id2="${ATUIN_HISTORY_ID:-}"
[[ -n "$id2" ]] || { echo "FAIL: second history_start"; exit 1; }
atuin_history_end "$id2" 0 0
echo "PASS: history_end (async)"

ATUIN_SEARCH_RESULT=""
atuin_search "echo hello zsh" 5 >/dev/null
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

ATUIN_NATIVE_VERSION=""
atuin_version >/dev/null
if [[ -n "${ATUIN_NATIVE_VERSION:-}" ]]; then
    echo "PASS: version (${ATUIN_NATIVE_VERSION})"
else
    echo "FAIL: version did not set ATUIN_NATIVE_VERSION"
    exit 1
fi

# The optional limit parser rejects malformed input.
if atuin_search "echo" "not-a-number" 2>/dev/null; then
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
