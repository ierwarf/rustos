#!/usr/bin/env bash
# Bounded CI smoke; sustained nightly campaigns should retain and merge corpora.
set -euo pipefail
repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
runs="${FORMAL_FUZZ_RUNS:-10000}"
wall_seconds="${FORMAL_FUZZ_WALL_SECONDS:-60}"
[[ "$runs" =~ ^[1-9][0-9]*$ ]] || { echo "FORMAL_FUZZ_RUNS must be positive" >&2; exit 2; }
[[ "$wall_seconds" =~ ^[1-9][0-9]*$ ]] || { echo "FORMAL_FUZZ_WALL_SECONDS must be positive" >&2; exit 2; }
command -v cargo-fuzz >/dev/null 2>&1 || { echo "missing cargo-fuzz; run: bash formal/setup-fuzz.sh" >&2; exit 2; }
mkdir -p build/formal/fuzz/corpus/rust build/formal/fuzz/corpus/c
if ! timeout --kill-after=5s "${wall_seconds}s" \
    cargo fuzz run --fuzz-dir formal/fuzz wire_and_image_admission \
        build/formal/fuzz/corpus/rust -- -runs="$runs" -print_final_stats=1 \
        >build/formal/fuzz/rust.log 2>&1; then
    tail -n 80 build/formal/fuzz/rust.log >&2
    exit 1
fi

cc="${CC:-clang}"
flags="$(pkg-config --cflags --libs gbm egl glesv2 libdrm)"
# shellcheck disable=SC2086
if ! "$cc" -O1 -g -ffunction-sections -fdata-sections -fsanitize=fuzzer,address,undefined \
    -Wl,--gc-sections \
    formal/fuzz-c/dvm_gpu_batch_fuzz.c $flags -o build/formal/fuzz/dvm-gpu-batch-fuzz \
    >build/formal/fuzz/c-build.log 2>&1; then
    tail -n 80 build/formal/fuzz/c-build.log >&2
    exit 1
fi
if ! timeout --kill-after=5s "${wall_seconds}s" \
    build/formal/fuzz/dvm-gpu-batch-fuzz build/formal/fuzz/corpus/c \
        -runs="$runs" -print_final_stats=1 >build/formal/fuzz/c.log 2>&1; then
    tail -n 80 build/formal/fuzz/c.log >&2
    exit 1
fi
jq -n --argjson runs "$runs" --argjson wall_seconds "$wall_seconds" \
    '{schema:"rustos-fuzz-evidence-v1",status:"passed",targets:["rust-wire-image-admission","c-dvm-gpu-batch"],runs_per_target:$runs,wall_limit_seconds_per_target:$wall_seconds,sanitizers:["address","undefined"]}' \
    >build/formal/fuzz/summary.json
printf 'coverage-guided fuzz smoke passed rust+c runs=%s each\n' "$runs"
