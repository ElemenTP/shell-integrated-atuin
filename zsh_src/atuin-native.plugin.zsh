# atuin-native.plugin.zsh — in-process Atuin shell history for zsh.
#
# A plugin-manager friendly loader, compatible with oh-my-zsh, zinit, antigen,
# zplug, zgen, sheldon, ... — point your plugin manager at this repository and
# the *.plugin.zsh naming convention makes it pick this file up.
#
# Before this plugin can do anything you must have built & installed the
# compiled module (see README):
#
#     cmake -B build -S .
#     cmake --build build --config Release
#     cmake --install build --config Release --prefix ~/.local
#
# The plugin mirrors the official atuin/src/shell/atuin.zsh script, replacing
# every `atuin <subcommand>` process spawn with an in-process builtin provided
# by the atuin_native zsh module (zmodload).

# Prevent double-loading
if (( ${+_ATUIN_NATIVE_LOADED} )); then
  return 0
fi

# Locate this script's directory (works under any plugin manager).
0="${${ZERO:-${0:#$ZSH_ARGZERO}}:-${(%):-%N}}"
0="${${(M)0:#/*}:-$PWD/$0}"
typeset -g _ATUIN_NATIVE_SCRIPT_DIR="${0:h}"

# ---- Compiled module discovery ---------------------------------------------
# zsh hardcodes the loadable-module suffix to ".so" via its DL_EXT macro on
# every platform, including macOS (zsh configure.ac: DL_EXT="${DL_EXT=so}").
_atuin_native_mods=(atuin_native.so)

case "${OSTYPE:-}" in
  darwin*)
    _atuin_native_ffis=(libatuin_ffi.dylib)
    ;;
  *)
    _atuin_native_ffis=(libatuin_ffi.so)
    ;;
esac

typeset -ga _atuin_native_dirs
if [[ -n "${ATUIN_NATIVE_DIR:-}" ]]; then
  _atuin_native_dirs+=("$ATUIN_NATIVE_DIR")
fi
_atuin_native_dirs+=(
  "$_ATUIN_NATIVE_SCRIPT_DIR"
  "${XDG_DATA_HOME:-$HOME/.local/share}/zsh/atuin-native"
  "$HOME/.local/lib/zsh/atuin-native"
  "/usr/local/lib/zsh/atuin-native"
  "/usr/lib/zsh/atuin-native"
  "/opt/atuin-native/lib/zsh/atuin-native"
)

# ATUIN_NATIVE_DIR is also the resolved module directory (exported so users
# can see where the module was loaded from).
typeset -g ATUIN_NATIVE_DIR=""

typeset _dir _mod _ffi _found=0
for _dir in "${_atuin_native_dirs[@]}"; do
  [[ -d "$_dir" ]] || continue
  for _mod in "${_atuin_native_mods[@]}"; do
    if [[ -f "$_dir/$_mod" ]]; then
      ATUIN_NATIVE_DIR="$_dir"
      _found=1
      for _ffi in "${_atuin_native_ffis[@]}"; do
        if [[ -f "$_dir/$_ffi" ]]; then
          break
        fi
      done
      break
    fi
  done
  (( _found )) && break
done
unset _ATUIN_NATIVE_SCRIPT_DIR _atuin_native_mods _atuin_native_ffis \
      _atuin_native_dirs _dir _mod _ffi _found

if [[ -z "$ATUIN_NATIVE_DIR" ]]; then
  print -u2 "atuin-native: compiled module (atuin_native) not found."
  print -u2 "  Build it first:"
  print -u2 "    cmake -B build -S . && cmake --build build --config Release"
  print -u2 "    cmake --install build --config Release --prefix \$HOME/.local"
  print -u2 "  Or point ATUIN_NATIVE_DIR at the installed lib/zsh directory."
  return 1
fi

# ---- Load the native module ------------------------------------------------
# ATUIN_SHELL is read by the Rust history builder while commands are recorded.
export ATUIN_SHELL="zsh"

module_path=("$ATUIN_NATIVE_DIR" $module_path)
zmodload atuin_native || {
  print -u2 "atuin-native: failed to load atuin_native from $ATUIN_NATIVE_DIR"
  return 1
}

typeset -g _ATUIN_NATIVE_LOADED=1

autoload -Uz add-zsh-hook
zmodload zsh/datetime 2>/dev/null

# ---- Session ID (replaces `ATUIN_SESSION=$(atuin uuid)`) -------------------
# The builtin writes $ATUIN_SESSION as a zsh parameter; command substitution is
# avoided on purpose because zsh forks for $() and the FFI fork guard rejects
# in-process calls from forked children.
if [[ -z "${ATUIN_SESSION:-}" || "${ATUIN_SHLVL:-}" != "$SHLVL" ]]; then
  atuin_session_id >/dev/null 2>&1 || {
    print -u2 "atuin-native: could not obtain session id"
    return 1
  }
  export ATUIN_SESSION="${ATUIN_SESSION:-}"
  export ATUIN_SHLVL="$SHLVL"
fi

ATUIN_HISTORY_ID=""

# ---- OSC 133 markers (optional, identical to official atuin.zsh) -----------
__atuin_osc133_command_executed() {
    [[ -n "${ATUIN_PTY_PROXY_ACTIVE:-}" ]] || return 0
    [[ -n "${ATUIN_HISTORY_ID:-}" ]] || return 0
    printf '\033]133;C\a'
}

__atuin_osc133_command_finished() {
    [[ -n "${ATUIN_PTY_PROXY_ACTIVE:-}" ]] || return 0
    [[ -n "${ATUIN_HISTORY_ID:-}" ]] || return 0
    printf '\033]133;D;%s;history_id=%s;session_id=%s\a' \
        "$1" "$ATUIN_HISTORY_ID" "${ATUIN_SESSION:-}"
}

__atuin_osc133_prompt_start=$'%{\033]133;A;cl=line\a%}'
__atuin_osc133_prompt_end=$'%{\033]133;B\a%}'

__atuin_osc133_wrap_prompt() {
    local __atuin_orig_prompt="${PROMPT-}"
    local __atuin_orig_rprompt="${RPROMPT-${RPS1-}}"

    local __atuin_prompt="$__atuin_orig_prompt"
    local __atuin_rprompt="$__atuin_orig_rprompt"
    __atuin_prompt="${__atuin_prompt//$__atuin_osc133_prompt_start/}"
    __atuin_prompt="${__atuin_prompt//$__atuin_osc133_prompt_end/}"
    __atuin_rprompt="${__atuin_rprompt//$__atuin_osc133_prompt_start/}"
    __atuin_rprompt="${__atuin_rprompt//$__atuin_osc133_prompt_end/}"

    if [[ -n "${ATUIN_PTY_PROXY_ACTIVE:-}" ]]; then
        PROMPT="${__atuin_osc133_prompt_start}${__atuin_prompt}"
        RPROMPT="${__atuin_rprompt}${__atuin_osc133_prompt_end}"
    else
        [[ "$__atuin_orig_prompt" == "$__atuin_prompt" ]] || PROMPT="$__atuin_prompt"
        [[ "$__atuin_orig_rprompt" == "$__atuin_rprompt" ]] || RPROMPT="$__atuin_rprompt"
    fi
}

# ---- Preexec hook (replaces `atuin history start --hook`) ------------------
# The builtin writes ATUIN_HISTORY_ID as a zsh parameter; stdout is discarded.
# NOTE: must NOT use $(...) here — command substitution forks and the fork
# guard would reject the call.
_atuin_native_preexec() {
    __atuin_preexec_time=${EPOCHREALTIME-}

    ATUIN_HISTORY_ID=""
    atuin_history_start "$1" "$PWD" >/dev/null 2>&1
    export ATUIN_HISTORY_ID="${ATUIN_HISTORY_ID:-}"

    __atuin_osc133_command_executed
}

# ---- Precmd hook (replaces `atuin history end &`) --------------------------
_atuin_native_precmd() {
    local EXIT="$?"

    __atuin_osc133_wrap_prompt

    [[ -z "${ATUIN_HISTORY_ID:-}" ]] && return

    local duration=""
    if [[ -n "${__atuin_preexec_time:-}" && -n "${EPOCHREALTIME:-}" ]]; then
        printf -v duration %.0f \
            $(( (EPOCHREALTIME - __atuin_preexec_time) * 1000000000 ))
    fi

    __atuin_osc133_command_finished "$EXIT"

    # Fire-and-forget (default), matching `(... atuin history end ... &)`.
    # Add --sync for debugging:
    #   atuin_history_end "$ATUIN_HISTORY_ID" "$EXIT" "${duration:-0}" --sync
    atuin_history_end "$ATUIN_HISTORY_ID" "$EXIT" "${duration:-0}"

    export ATUIN_HISTORY_ID=""
}

# ---- Comment-line history (replaces the official zshaddhistory hook) -------
# With interactive_comments, a line starting with '#' is added to history
# without executing anything, so preexec never fires for it.
_atuin_native_zshaddhistory() {
    [[ -o interactive_comments ]] || return 0
    local line=${1%$'\n'}
    [[ $line == \#* && $line != *$'\n'* ]] || return 0

    local saved_id="${ATUIN_HISTORY_ID:-}"
    ATUIN_HISTORY_ID=""
    atuin_history_start "$line" "$PWD" >/dev/null 2>&1
    local id="${ATUIN_HISTORY_ID:-}"
    [[ -n "$id" ]] && atuin_history_end "$id" 0 0
    ATUIN_HISTORY_ID="$saved_id"
    return 0
}

# ---- Register hooks --------------------------------------------------------
# Only in interactive shells. In scripts preexec fires before every source
# statement with an empty command, which would pollute history while running
# the test suite.
if [[ -o interactive ]]; then
    # Allow comment lines at the interactive prompt (matches official atuin).
    setopt interactive_comments

    add-zsh-hook preexec _atuin_native_preexec
    add-zsh-hook precmd _atuin_native_precmd
    add-zsh-hook zshaddhistory _atuin_native_zshaddhistory
fi

# ---- Autosuggest integration (replaces `atuin search --cmd-only --limit 1`)
# Uses the ATUIN_SEARCH_RESULT parameter (set by the atuin_search builtin) to
# avoid command substitution fork, which corrupts the in-process tokio runtime.
if (( ${+ZSH_AUTOSUGGEST_STRATEGY} )); then
    _zsh_autosuggest_strategy_atuin_native() {
        atuin_search "$1" 1 2>/dev/null
        typeset -g suggestion="${ATUIN_SEARCH_RESULT:-}"
    }

    ZSH_AUTOSUGGEST_STRATEGY=("atuin_native" "${ZSH_AUTOSUGGEST_STRATEGY[@]}")
fi

# ---- Interactive search widgets -------------------------------------------
# The widget calls the in-process full-screen TUI (official
# `atuin search -i` replacement). The builtin runs inside the shell process —
# no fork, no external atuin binary — and writes the selection to
# $ATUIN_SEARCH_SELECTED, so the widget never needs command substitution.
_atuin_native_search() {
    emulate -L zsh
    zle -I

    local __atuin_status
    atuin_search_interactive "$BUFFER"
    __atuin_status=$?

    zle reset-prompt
    # The TUI switches the terminal to raw mode; zsh had already enabled
    # bracketed paste, so restore it exactly like the official widget does
    # after the external TUI exits.
    [[ -n ${zle_bracketed_paste[1]:-} ]] &&
        printf '%s' "${zle_bracketed_paste[1]}" >/dev/tty

    if (( __atuin_status == 0 )); then
        local output="${ATUIN_SEARCH_SELECTED:-}"
        if [[ -n "$output" ]]; then
            RBUFFER=""
            LBUFFER="$output"

            if [[ $LBUFFER == __atuin_accept__:* ]]; then
                LBUFFER=${LBUFFER#__atuin_accept__:}
                zle accept-line
            fi
        fi
    fi
    return 0
}
_atuin_native_search_vicmd() { _atuin_native_search "$@"; }
_atuin_native_search_viins() { _atuin_native_search "$@"; }

_atuin_native_up_search() {
    if [[ ! "$BUFFER" == *$'\n'* ]]; then
        _atuin_native_search "$@"
    else
        zle up-line
    fi
}
_atuin_native_up_search_vicmd() { _atuin_native_up_search "$@"; }
_atuin_native_up_search_viins() { _atuin_native_up_search "$@"; }

if [[ -o interactive ]]; then
    zle -N atuin-search _atuin_native_search
    zle -N atuin-search-vicmd _atuin_native_search_vicmd
    zle -N atuin-search-viins _atuin_native_search_viins
    zle -N atuin-up-search _atuin_native_up_search
    zle -N atuin-up-search-vicmd _atuin_native_up_search_vicmd
    zle -N atuin-up-search-viins _atuin_native_up_search_viins

    # Compatibility widget names for atuin <= 17.2.1 users.
    zle -N _atuin_search_widget _atuin_native_search
    zle -N _atuin_up_search_widget _atuin_native_up_search

    # Same default key bindings as `atuin init zsh` (see atuin_orig.zsh).
    # The official init script binds these unconditionally; without them the
    # widgets are registered but Ctrl+R / Up never reach the TUI.
    bindkey -M emacs '^r' atuin-search
    bindkey -M viins '^r' atuin-search-viins
    bindkey -M vicmd '/' atuin-search
    bindkey -M emacs '^[[A' atuin-up-search
    bindkey -M vicmd '^[[A' atuin-up-search-vicmd
    bindkey -M viins '^[[A' atuin-up-search-viins
    bindkey -M emacs '^[OA' atuin-up-search
    bindkey -M vicmd '^[OA' atuin-up-search-vicmd
    bindkey -M viins '^[OA' atuin-up-search-viins
    bindkey -M vicmd 'k' atuin-up-search-vicmd
fi
