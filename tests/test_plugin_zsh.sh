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

# atuin_init takes no data-dir argument any more: the session
# resolves its paths from the settings tree. Pin data_dir in an isolated
# config.toml so the tests never touch the real ~/.config/atuin.
export ATUIN_CONFIG_DIR="$ATUIN_DATA_DIR/config"
mkdir -p "$ATUIN_CONFIG_DIR"
printf 'data_dir = "%s"\n' "$ATUIN_DATA_DIR" > "$ATUIN_CONFIG_DIR/config.toml"
export ATUIN_NATIVE_DIR="$MODULE_DIR"
export ATUIN_SESSION=""

# Make the plugin install its autosuggest strategy so the test can call it.
ZSH_AUTOSUGGEST_STRATEGY=(default)

source "$PLUGIN"

[[ -n "${_ATUIN_NATIVE_LOADED:-}" ]] && echo "PASS: plugin loaded" \
    || { echo "FAIL: plugin did not set _ATUIN_NATIVE_LOADED"; exit 1; }
[[ -n "${ATUIN_SESSION:-}" ]] && echo "PASS: session id exported" \
    || { echo "FAIL: ATUIN_SESSION not exported"; exit 1; }
# zsh-autosuggestions parity: the strategy must be installed and prepended,
# whether or not zsh-autosuggestions has already been loaded.
[[ "${+functions[_zsh_autosuggest_strategy_atuin_native]}" == 1 ]] && echo "PASS: autosuggest strategy installed" \
    || { echo "FAIL: autosuggest strategy missing"; exit 1; }
[[ "${ZSH_AUTOSUGGEST_STRATEGY[1]}" == "atuin_native" ]] && echo "PASS: autosuggest strategy prepended" \
    || { echo "FAIL: ZSH_AUTOSUGGEST_STRATEGY=${ZSH_AUTOSUGGEST_STRATEGY[*]}"; exit 1; }

# Sourcing before zsh-autosuggestions (variable unset) must still configure it.
autosuggest_default=$(env -u ZSH_AUTOSUGGEST_STRATEGY \
    ATUIN_NATIVE_DIR="$ATUIN_NATIVE_DIR" \
    ATUIN_DATA_DIR="$ATUIN_DATA_DIR" \
    ATUIN_CONFIG_DIR="$ATUIN_CONFIG_DIR" \
    zsh -c "source '$PLUGIN'; print -r -- \"\${ZSH_AUTOSUGGEST_STRATEGY[*]}\"")
[[ "$autosuggest_default" == "atuin_native" ]] \
    && echo "PASS: autosuggest configured before zsh-autosuggestions loads" \
    || { echo "FAIL: unset strategy produced '$autosuggest_default'"; exit 1; }

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
ATUIN_SEARCH_QUERY="echo plugin-hook-test"
ATUIN_SEARCH_LIMIT=1
atuin_search >/dev/null
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

# ---- Output-capture markers (atuin 18.24 protocol) -------------------------
# atuin 18.24 replaced OSC 133 with its own sequence (18188735); a proxy that
# only knows the old protocol still gets OSC 133, keyed off
# ATUIN_PTY_PROXY_ACTIVE=1. Runs in a subshell so the marker state is fresh.
capture_markers() {
    local -x ATUIN_PTY_PROXY_ACTIVE="$1"
    local -x ATUIN_HISTORY_ID="test-id"
    __atuin_pty_proxy_owns_tty=1
    __atuin_mark_output_start
    __atuin_mark_output_end 7
}

new_proxy=$(capture_markers 2)
[[ "$new_proxy" == *$'\033]18188735;C;test-id\a'* &&
   "$new_proxy" == *$'\033]18188735;D;test-id\a'* &&
   "$new_proxy" != *$'\033]133;'* ]] \
    && echo "PASS: new pty-proxy uses the 18.24 marker sequence" \
    || { echo "FAIL: new marker sequence = ${new_proxy@q}"; exit 1; }

old_proxy=$(capture_markers 1)
[[ "$old_proxy" == *$'\033]133;C\a'* &&
   "$old_proxy" == *$'\033]133;D;7;history_id=test-id\a'* ]] \
    && echo "PASS: old pty-proxy still gets OSC 133" \
    || { echo "FAIL: old marker sequence = ${old_proxy@q}"; exit 1; }

# A filtered command has no history id: no C/D markers (but an old proxy still
# needs its capture reset).
no_id=$(
    local -x ATUIN_PTY_PROXY_ACTIVE=2
    local -x ATUIN_HISTORY_ID=""
    __atuin_pty_proxy_owns_tty=1
    __atuin_mark_output_start
    __atuin_mark_output_end 0
)
[[ -z "$no_id" ]] \
    && echo "PASS: no markers without a history id" \
    || { echo "FAIL: unexpected markers ${no_id@q}"; exit 1; }

reset=$(
    local -x ATUIN_PTY_PROXY_ACTIVE=1
    local -x ATUIN_HISTORY_ID="test-id"
    __atuin_pty_proxy_owns_tty=1
    __atuin_needs_osc133_reset=1
    __atuin_mark_output_start
)
[[ "$reset" == $'\033]133;B\a'* && "$reset" == *$'\033]133;C\a'* ]] \
    && echo "PASS: old pty-proxy capture is reset before the next command" \
    || { echo "FAIL: reset marker = ${reset@q}"; exit 1; }

# No proxy: no markers at all, and the prompt is left untouched (18.24 no
# longer wraps PROMPT/RPROMPT with OSC 133 prompt markers).
__atuin_pty_proxy_owns_tty=0
PROMPT='prompt>' RPROMPT='<rprompt'
ATUIN_HISTORY_ID=""
_atuin_native_precmd
[[ "$PROMPT" == 'prompt>' && "$RPROMPT" == '<rprompt' ]] \
    && echo "PASS: prompt is not wrapped for output capture" \
    || { echo "FAIL: PROMPT=${PROMPT@q} RPROMPT=${RPROMPT@q}"; exit 1; }

# ---- Binding settings (atuin 18.24.5) --------------------------------------
# `bind_ctrl_r` / `bind_up_arrow` decide whether `atuin init` binds each chord;
# the plugin asks the session for the resolved values.
atuin_bindings
[[ "${ATUIN_BIND_CTRL_R:-}" == 1 && "${ATUIN_BIND_UP_ARROW:-}" == 1 ]] \
    && echo "PASS: bindings default to ctrl-r and up-arrow" \
    || { echo "FAIL: bindings = ${ATUIN_BIND_CTRL_R:-} ${ATUIN_BIND_UP_ARROW:-}"; exit 1; }

bindings_off=$(env ATUIN_BIND_CTRL_R=false \
    ATUIN_DATA_DIR="$ATUIN_DATA_DIR" ATUIN_CONFIG_DIR="$ATUIN_CONFIG_DIR" \
    ATUIN_NATIVE_DIR="$ATUIN_NATIVE_DIR" \
    zsh -c 'module_path=($ATUIN_NATIVE_DIR $module_path); zmodload atuin_native; atuin_bindings; print -r -- "$ATUIN_BIND_CTRL_R $ATUIN_BIND_UP_ARROW"')
[[ "$bindings_off" == "0 1" ]] \
    && echo "PASS: bind_ctrl_r=false disables ctrl-r only" \
    || { echo "FAIL: bindings with ATUIN_BIND_CTRL_R=false = ${bindings_off@q}"; exit 1; }

zmodload -u atuin_native
echo "PASS: module unloaded"

rm -rf "$ATUIN_DATA_DIR"
echo "=== All plugin integration tests passed ==="
