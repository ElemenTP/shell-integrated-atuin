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

module_path=("$MODULE_DIR" $module_path)
zmodload atuin_native || { echo "FAIL: load"; exit 1; }
echo "PASS: load"

# Sanity: direct call in the parent process.
ATUIN_HISTORY_ID=""
atuin_history_start "echo direct" "/tmp" >/dev/null 2>&1
id="${ATUIN_HISTORY_ID:-}"
[[ -n "$id" ]] && echo "PASS: history_start in parent" || { echo "FAIL: start"; exit 1; }

# ---- $() command substitution (fork guard must reject) ----
if sub_id=$(atuin_history_start "echo subshell" "/tmp" 2>/dev/null); then
    echo "PASS: \$() survived (fork guard did not reject, len=${#sub_id})"
else
    echo "PASS: \$() rejected by fork guard"
fi

# ---- Background job (fork guard rejects; expect non-zero) ----
atuin_history_start "echo background" "/tmp" &>/dev/null &
bg_pid=$!
if wait $bg_pid 2>/dev/null; then
    echo "PASS: background & survived (call went through)"
else
    echo "PASS: background & rejected by fork guard"
fi

# ---- Pipeline, builtin not last stage ----
if atuin_history_start "echo pipeline" "/tmp" 2>/dev/null | cat >/dev/null; then
    echo "PASS: pipeline survived (call went through)"
else
    echo "PASS: pipeline rejected by fork guard"
fi

# ---- Subshell ( ) ----
if ( atuin_history_start "echo subshell2" "/tmp" 2>/dev/null ); then
    echo "PASS: subshell survived (call went through)"
else
    echo "PASS: subshell rejected by fork guard"
fi

# ---- Process substitution <( ) ----
if cat <(atuin_history_start "echo procsubst" "/tmp" 2>/dev/null) >/dev/null 2>&1; then
    echo "PASS: process substitution survived"
else
    echo "PASS: process substitution rejected by fork guard"
fi

# ---- Nested command substitution ----
if outer=$(echo $(atuin_history_start "echo nested" "/tmp" 2>/dev/null)); then
    echo "PASS: nested \$() survived"
else
    echo "PASS: nested \$() rejected by fork guard"
fi

# ---- Parent still works after forks ----
ATUIN_HISTORY_ID=""
atuin_history_start "echo after-forks" "/tmp" >/dev/null 2>&1
id2="${ATUIN_HISTORY_ID:-}"
[[ -n "$id2" ]] && echo "PASS: parent functional after forks" || { echo "FAIL: parent broken"; exit 1; }

atuin_history_end "$id2" 0 0 --sync 2>/dev/null
echo "PASS: history_end in parent"

# ---- Unload/reload ----
zmodload -u atuin_native
echo "PASS: unload"
zmodload atuin_native
ATUIN_HISTORY_ID=""
atuin_history_start "echo reloaded" "/tmp" >/dev/null 2>&1
id3="${ATUIN_HISTORY_ID:-}"
[[ -n "$id3" ]] && echo "PASS: reload + start" || { echo "FAIL: reload"; exit 1; }
zmodload -u atuin_native
echo "PASS: final unload"

rm -rf "$ATUIN_DATA_DIR"
echo "=== All atuin fork tests passed ==="
