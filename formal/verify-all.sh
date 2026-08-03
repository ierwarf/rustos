#!/usr/bin/env bash
# Tiered formal gate. PR is exhaustive and deterministic; nightly adds bug-finding lanes.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
profile="pr"
if [[ "${1:-}" == "--profile" ]]; then
    profile="${2:-}"
    shift 2
fi
[[ "$#" -eq 0 ]] || { echo "usage: $0 [--profile pr|nightly]" >&2; exit 2; }
case "$profile" in
    pr|nightly) ;;
    *) echo "invalid formal profile: $profile" >&2; exit 2 ;;
esac

verification_dir="$repo_root/build/formal/verification-run"
mkdir -p "$verification_dir"
run_marker="$(mktemp "$verification_dir/$profile.started.XXXXXX")"
trap 'rm -f "$run_marker"' EXIT

lane_dir="$verification_dir/$profile-lanes"
mkdir -p "$lane_dir"

run_parallel_lane() {
    local name="$1"
    shift
    "$@" >"$lane_dir/$name.log" 2>&1 &
    lane_names+=("$name")
    lane_pids+=("$!")
}

wait_parallel_lanes() {
    local failed=0
    local index
    for index in "${!lane_pids[@]}"; do
        if wait "${lane_pids[$index]}"; then
            cat "$lane_dir/${lane_names[$index]}.log"
        else
            printf 'formal lane failed: %s\n' "${lane_names[$index]}" >&2
            tail -n 80 "$lane_dir/${lane_names[$index]}.log" >&2
            failed=1
        fi
    done
    [[ "$failed" -eq 0 ]]
}

bash formal/selftest.sh
bash formal/run-proof-index.sh
declare -a lane_names=()
declare -a lane_pids=()
# These gates have disjoint evidence directories and no logical dependency on
# one another. Running them concurrently keeps the complete PR proof surface
# while bounding wall time; each lane remains fail-closed and its output is
# replayed only after the exact child status is collected.
run_parallel_lane source-conformance bash formal/run-source-conformance.sh
run_parallel_lane tlc env FORMAL_SELFTEST_ALREADY_PASSED=1 \
    bash formal/run-all-tlc.sh --profile "$profile"
run_parallel_lane spec-mutations bash formal/run-spec-mutations.sh
run_parallel_lane fault-scenarios bash formal/run-fault-scenarios.sh
run_parallel_lane abi-differential bash formal/run-abi-differential.sh
run_parallel_lane recovery-scenarios bash formal/run-recovery-scenarios.sh
run_parallel_lane implementation-mutations bash formal/run-implementation-mutations.sh
run_parallel_lane kani env FORMAL_PROOF_INDEX_ALREADY_PASSED=1 bash formal/run-kani.sh
run_parallel_lane verus env FORMAL_PROOF_INDEX_ALREADY_PASSED=1 bash formal/run-verus.sh
run_parallel_lane concurrency-triangle bash formal/run-concurrency-triangle.sh
run_parallel_lane runtime-traces bash formal/run-runtime-traces.sh
wait_parallel_lanes
if [[ "$profile" == nightly ]]; then
    bash formal/run-sanitizers.sh --profile=all
    bash formal/run-miri.sh
    bash formal/run-apalache.sh
    bash formal/run-tlaps.sh
    bash formal/run-fuzz-smoke.sh
fi
python3 formal/write-verification-run.py \
    --root "$repo_root" \
    --profile "$profile" \
    --not-before "$run_marker" \
    --output "$repo_root/build/formal/verification-run/$profile.json"
