#!/usr/bin/env bash
# Tiered formal gate. PR is exhaustive and deterministic; nightly adds bug-finding lanes.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
gate_started="$SECONDS"
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

# An exact source-tree seal already binds every required artifact by content
# digest. Rechecking those digests is equivalent to rerunning an unchanged
# gate and avoids rebuilding hundreds of mutation witnesses after commands
# that only consume, rather than modify, the checkout.
if python3 formal/reuse-verification-run.py --root "$repo_root" --profile "$profile"; then
    exit 0
fi

# Keep warm Cargo/proof caches unless the filesystem is genuinely close to
# exhaustion. The old 20 GiB trigger discarded several gigabytes on this
# workstation before every PR gate even though one complete four-shard run
# needs less than half of that headroom. Operators can still raise the floor
# explicitly for a smaller or shared volume.
# A warm mutation target is the dominant cache and is also required by this
# very gate. Deleting it to cross the cold-start floor only recreates the same
# bytes and turns every verification into a multi-minute rebuild. Once that
# exact lane cache exists, retain it and require only incremental headroom.
if [[ -d "$repo_root/build/formal/implementation-mutations/target" ]]; then
    default_reclaim_threshold_kb=$((512 * 1024))
else
    default_reclaim_threshold_kb=$((4 * 1024 * 1024))
fi
reclaim_threshold_kb="${RUSTOS_FORMAL_RECLAIM_THRESHOLD_KB:-$default_reclaim_threshold_kb}"
available_kb="$(df -Pk "$repo_root" | awk 'NR==2 {print $4}')"
if [[ -n "$available_kb" && "$available_kb" -lt "$reclaim_threshold_kb" ]]; then
    printf 'formal: %s KiB free, reclaiming regenerable lane caches\n' "$available_kb"
    bash "$repo_root/tools/reclaim-build-space.sh" || true
fi

verification_dir="$repo_root/build/formal/verification-run"
mkdir -p "$verification_dir"
run_marker="$(mktemp "$verification_dir/$profile.started.XXXXXX")"
trap 'rm -f "$run_marker"' EXIT

lane_dir="$verification_dir/$profile-lanes"
mkdir -p "$lane_dir"

run_parallel_lane() {
    local name="$1"
    shift
    # Each lane records its own wall time, because the gate's cost is one
    # lane's cost: everything else finishes inside the slowest one, and
    # without this the only way to find that lane is to compare log mtimes.
    rm -f "$lane_dir/$name.seconds"
    (
        lane_started="$SECONDS"
        # `|| lane_status=$?` exempts the lane from the inherited `set -e`.
        # Without it the subshell dies on a failing lane before recording
        # anything, and a failure is exactly when its cost is worth knowing.
        lane_status=0
        "$@" >"$lane_dir/$name.log" 2>&1 || lane_status=$?
        printf '%s\n' "$((SECONDS - lane_started))" >"$lane_dir/$name.seconds"
        exit "$lane_status"
    ) &
    lane_names+=("$name")
    lane_pids+=("$!")
}

lane_seconds() {
    local file="$lane_dir/$1.seconds"
    [[ -s "$file" ]] && cat "$file" || printf 'unknown'
}

wait_parallel_lanes() {
    local failed=0
    local index
    for index in "${!lane_pids[@]}"; do
        if wait "${lane_pids[$index]}"; then
            # The full lane log is retained under build/formal/ regardless; a
            # passing lane has nothing an agent or reviewer needs to see, so
            # stdout stays to the one-line summary instead of replaying every
            # lane's complete log on every green run.
            printf 'formal lane passed: %s elapsed_seconds=%s\n' \
                "${lane_names[$index]}" "$(lane_seconds "${lane_names[$index]}")"
        else
            printf 'formal lane failed: %s elapsed_seconds=%s\n' \
                "${lane_names[$index]}" "$(lane_seconds "${lane_names[$index]}")" >&2
            tail -n 80 "$lane_dir/${lane_names[$index]}.log" >&2
            failed=1
        fi
    done
    [[ "$failed" -eq 0 ]]
}

bash formal/selftest.sh
bash formal/run-proof-index.sh
# The exhaustive TLC set is the only lane whose contract is a wall clock.
# `tlc_max_wall_seconds` and each model's pinned per-model timeout are budgets
# measured in real seconds, so they mean what they say only when the lane is
# not competing with ten siblings for the same cores: a model that explores its
# state space in 16 seconds against a 30-second timeout starts failing on load
# instead of on logic. It runs first, with the machine to itself. When its
# evidence is reusable that costs a few seconds, and when a model genuinely has
# to run, it runs under the conditions its budget was pinned for. It also
# leaves the exact baselines the mutation lane below reuses.
tlc_started="$SECONDS"
FORMAL_SELFTEST_ALREADY_PASSED=1 bash formal/run-all-tlc.sh --profile "$profile"
printf 'formal lane passed: tlc elapsed_seconds=%s\n' "$((SECONDS - tlc_started))"
declare -a lane_names=()
declare -a lane_pids=()
# These gates have disjoint evidence directories and no logical dependency on
# one another. Running them concurrently keeps the complete PR proof surface
# while bounding wall time; each lane remains fail-closed and its output is
# replayed only after the exact child status is collected.
run_parallel_lane source-conformance bash formal/run-source-conformance.sh
run_parallel_lane spec-mutations bash formal/run-spec-mutations.sh
run_parallel_lane fault-scenarios bash formal/run-fault-scenarios.sh
run_parallel_lane abi-differential bash formal/run-abi-differential.sh
run_parallel_lane recovery-scenarios bash formal/run-recovery-scenarios.sh
if [[ "$profile" == nightly ]]; then
    run_parallel_lane implementation-mutations env \
        RUSTOS_IMPLEMENTATION_MUTATION_CACHE=off \
        bash formal/run-implementation-mutations.sh
else
    run_parallel_lane implementation-mutations bash formal/run-implementation-mutations.sh
fi
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
printf 'formal gate sealed profile=%s elapsed_seconds=%s\n' \
    "$profile" "$((SECONDS - gate_started))"
