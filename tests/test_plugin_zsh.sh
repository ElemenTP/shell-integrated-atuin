#!/usr/bin/env zsh
# Integration test for zsh_src/atuin-native.plugin.zsh.
#
# Sources the real plugin (as a plugin manager would), then drives its hook
# functions directly. This validates the parameter-based protocol used to
# avoid command-substitution forks.
#
# Usage:
#   MODULE_DIR=/path/to/zsh_src/build zsh tests/test_plugin_zsh.sh

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$PWD}"
PLUGIN="${PLUGIN:-$REPO_ROOT/atuin-native.plugin.zsh}"
MODULE_DIR="${MODULE_DIR:-$REPO_ROOT/zsh_src/build}"
ATUIN_DATA_DIR="${ATUIN_DATA_DIR:-/tmp/atuin-plugin-test-$$}"

echo "=== atuin zsh plugin integration test ==="
echo "Plugin: $PLUGIN"
echo "Module dir: $MODULE_DIR"

mkdir -p "$ATUIN_DATA_DIR"
export ATUIN_DATA_DIR
export ATUIN_NATIVE_DIR="$MODULE_DIR"
export ATUIN_SESSION=""

# Make the plugin install its autosuggest strategy so the test can call it.
ZSH_AUTOSUGGEST_STRATEGY=(default)

source "$PLUGIN"

[[ -n "${_ATUIN_NATIVE_LOADED:-}" ]] && echo "PASS: plugin loaded" \
    || { echo "FAIL: plugin did not set _ATUIN_NATIVE_LOADED"; exit 1; }
[[ -n "${ATUIN_SESSION:-}" ]] && echo "PASS: session id exported" \
    || { echo "FAIL: ATUIN_SESSION not exported"; exit 1; }
[[ "${+functions[_zsh_autosuggest_strategy_atuin_native]}" == 1 ]] && echo "PASS: autosuggest strategy installed" \
    || { echo "FAIL: autosuggest strategy missing"; exit 1; }

# A second source must be a no-op.
typeset -g _ATUIN_NATIVE_LOADED=1
source "$PLUGIN" && echo "PASS: double-source is a no-op"

# Drive the preexec hook exactly as zsh would for an executed command.
ATUIN_HISTORY_ID=""
_atuin_native_preexec "echo plugin-hook-test"
id="${ATUIN_HISTORY_ID:-}"
[[ -n "$id" ]] && echo "PASS: preexec recorded history start" \
    || { echo "FAIL: preexec did not set ATUIN_HISTORY_ID"; exit 1; }

# precmd reads $? and duration state, then fires the async history end.
_atuin_native_precmd
[[ -z "${ATUIN_HISTORY_ID:-}" ]] && echo "PASS: precmd cleared ATUIN_HISTORY_ID" \
    || { echo "FAIL: precmd did not clear ATUIN_HISTORY_ID"; exit 1; }

# The started entry is already searchable before the async end completes.
ATUIN_SEARCH_RESULT=""
atuin_search "echo plugin-hook-test" 1 >/dev/null
[[ "${ATUIN_SEARCH_RESULT:-}" == *"echo plugin-hook-test"* ]] \
    && echo "PASS: native search finds hook-recorded command" \
    || { echo "FAIL: native search missed hook-recorded command"; exit 1; }

# Autosuggest strategy contract: sets global $suggestion without forking.
suggestion=""
_zsh_autosuggest_strategy_atuin_native "echo plugin-hook-test"
[[ "${suggestion:-}" == *"echo plugin-hook-test"* ]] \
    && echo "PASS: autosuggest strategy sets suggestion" \
    || { echo "FAIL: autosuggest strategy returned no suggestion"; exit 1; }

# Comment lines are recorded through zshaddhistory (preexec does not run).
ATUIN_HISTORY_ID=""
_atuin_native_zshaddhistory $'# plugin comment\n'
[[ -z "${ATUIN_HISTORY_ID:-}" ]] && echo "PASS: zshaddhistory preserves ATUIN_HISTORY_ID" \
    || { echo "FAIL: zshaddhistory clobbered ATUIN_HISTORY_ID"; exit 1; }

zmodload -u atuin_native
echo "PASS: module unloaded"

rm -rf "$ATUIN_DATA_DIR"
echo "=== All plugin integration tests passed ==="
