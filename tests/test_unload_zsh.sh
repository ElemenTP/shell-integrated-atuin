#!/usr/bin/env zsh
# Module unload/load cycle stress test for atuin_native.
#
# Verifies that repeated zmodload -u / zmodload cycles do not crash, leak
# threads, or leave corrupted state. Each cycle exercises the tokio runtime
# shutdown path (cleanup_ -> atuin_session_destroy -> Session::drop).
#
# Usage:
#   MODULE_DIR=/path/to/zsh_src/build zsh tests/test_unload_zsh.sh

set -euo pipefail

MODULE_DIR="${MODULE_DIR:-$PWD/zsh_src/build}"
ATUIN_DATA_DIR="${ATUIN_DATA_DIR:-/tmp/atuin-unload-test-$$}"
CYCLES="${CYCLES:-5}"

echo "=== atuin unload/load cycle test ==="
echo "Module dir: $MODULE_DIR"
echo "Cycles: $CYCLES"

mkdir -p "$ATUIN_DATA_DIR"
export ATUIN_DATA_DIR
module_path=("$MODULE_DIR" $module_path)

baseline_threads=0
loaded_threads=0
if [[ -d /proc/$$/task ]]; then
    baseline_threads=$(ls /proc/$$/task 2>/dev/null | wc -l)
    echo "Baseline threads: $baseline_threads"
fi

for ((i=1; i<=CYCLES; i++)); do
    zmodload atuin_native || { echo "FAIL: load cycle $i"; exit 1; }

    ATUIN_HISTORY_ID=""
    atuin_history_start "echo unload-cycle-$i" "/tmp" >/dev/null 2>&1
    id="${ATUIN_HISTORY_ID:-}"
    [[ -n "$id" ]] || { echo "FAIL: start cycle $i"; exit 1; }
    # Exercise the real precmd path (fire-and-forget, not --sync) so tokio and
    # sqlx spawn their worker threads.
    atuin_history_end "$id" 0 0 2>/dev/null
    sleep 0.1

    if [[ -d /proc/$$/task && i -eq 1 ]]; then
        loaded_threads=$(ls /proc/$$/task 2>/dev/null | wc -l)
        echo "Loaded threads: $loaded_threads (baseline: $baseline_threads)"
        (( loaded_threads >= baseline_threads + 2 )) || {
            echo "FAIL: multi-thread runtime did not spawn worker threads"
            exit 1
        }
    fi

    zmodload -u atuin_native || { echo "FAIL: unload cycle $i"; exit 1; }
    echo "PASS: cycle $i (load -> async end -> unload)"
done

if [[ -d /proc/$$/task ]]; then
    sleep 0.5
    after_threads=$(ls /proc/$$/task 2>/dev/null | wc -l)
    echo "Threads after $CYCLES cycles: $after_threads (baseline: $baseline_threads)"
    if (( after_threads - baseline_threads <= 2 )); then
        echo "PASS: no leaked tokio/sqlx threads after cycles"
    else
        echo "FAIL: $((after_threads - baseline_threads)) extra threads after cycles"
        exit 1
    fi
else
    echo "SKIP: thread-count check (no /proc/$$/task)"
fi

rm -rf "$ATUIN_DATA_DIR"
echo "=== All unload tests passed ==="
