#!/usr/bin/env bash
# Native behavioral witnesses: no real device, VFIO binding or GPU is opened.
set -euo pipefail
ulimit -c 0
repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
out="${FORMAL_DVM_TIMEOUT_ARTIFACT_DIR:-$repo_root/build/formal/dvm-timeouts}"
mkdir -p "$out"
compiler="${HOSTCC:-cc}"
flags=(-std=c11 -O2 -g -Wall -Wextra -Werror -pthread)
gpu_flags_text="$(pkg-config --cflags --libs libdrm gbm egl glesv2)"
read -r -a gpu_flags <<<"$gpu_flags_text"
block=driver-domains/linux/package/rustos-dvm-block
display=driver-domains/linux/package/rustos-dvm-display
"$compiler" "${flags[@]}" "-DREADY_DIR=\"$out/ready\"" \
    "$block/src/rustos-dvm-block.c" -o "$out/block-selftest"
"$compiler" "${flags[@]}" "$block/tests/watchdog.c" -o "$out/block-watchdog"
"$compiler" "${flags[@]}" "$display/tests/recovery.c" \
    "$display/src/rustos-dvm-gpu-runtime.c" "${gpu_flags[@]}" \
    -o "$out/gpu-recovery"
run_test() {
    local name="$1"
    shift
    local log="$out/$name.log"
    : > "$log"
    # The relay reopens stderr with O_APPEND. Keep the inherited descriptors
    # append-only too, or buffered stdout could overwrite its diagnostic bytes.
    if timeout --kill-after=2s 20s "$@" >> "$log" 2>&1; then
        printf 'DVM native witness passed: %s\n' "$name"
    else
        local status=$?
        tail -n 80 "$log" >&2
        return "$status"
    fi
}
run_test block-selftest "$out/block-selftest" selftest
run_test block-watchdog "$out/block-watchdog"
run_test gpu-recovery "$out/gpu-recovery"
printf 'DVM native deadline, quarantine, completion and watchdog witnesses passed\n'
