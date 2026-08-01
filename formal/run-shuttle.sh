#!/usr/bin/env bash
# Run bounded PCT schedule exploration for source-anchored concurrency flows.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
artifact_dir="${SHUTTLE_ARTIFACT_DIR:-$repo_root/build/formal/shuttle}"
mkdir -p "$artifact_dir"

python3 formal/check-concurrency-triangle.py

read -r default_iterations default_depth default_seconds < <(
    python3 - <<'PY'
import tomllib
with open("formal/concurrency-triangle.toml", "rb") as handle:
    budget = tomllib.load(handle)["budget"]
print(budget["shuttle_iterations"], budget["shuttle_pct_depth"], budget["shuttle_max_seconds"])
PY
)
iterations="${SHUTTLE_ITERATIONS:-$default_iterations}"
depth="${SHUTTLE_PCT_DEPTH:-$default_depth}"
seconds="${SHUTTLE_MAX_SECONDS:-$default_seconds}"
[[ "$iterations" =~ ^[0-9]+$ && "$iterations" -ge 16 && "$iterations" -le 2048 ]] || {
    echo "SHUTTLE_ITERATIONS must be in 16..2048" >&2
    exit 2
}
[[ "$depth" =~ ^[0-9]+$ && "$depth" -ge 1 && "$depth" -le 4 ]] || {
    echo "SHUTTLE_PCT_DEPTH must be in 1..4" >&2
    exit 2
}
[[ "$seconds" =~ ^[0-9]+$ && "$seconds" -ge 1 && "$seconds" -le 120 ]] || {
    echo "SHUTTLE_MAX_SECONDS must be in 1..120" >&2
    exit 2
}

mapfile -t cases < <(
    python3 - <<'PY'
import tomllib
with open("formal/concurrency-triangle.toml", "rb") as handle:
    for scenario in tomllib.load(handle)["scenario"]:
        print(f"{scenario['id']}\t{scenario['shuttle_test']}\t{scenario['source']}")
PY
)

result_dir="$artifact_dir/results"
rm -rf "$result_dir"
mkdir -p "$result_dir"
for case in "${cases[@]}"; do
    IFS=$'\t' read -r ident test_name source <<<"$case"
    log="$result_dir/$ident.log"
    if ! timeout --preserve-status "$seconds" env \
        SHUTTLE_ITERATIONS="$iterations" \
        SHUTTLE_PCT_DEPTH="$depth" \
        CARGO_TARGET_DIR="$artifact_dir/target" \
        cargo test -q --manifest-path formal/shuttle-proof-kernel/Cargo.toml "tests::$test_name" -- --exact \
        >"$log" 2>&1; then
        tail -n 80 "$log" >&2
        exit 1
    fi
    rg -q '^running 1 test$' "$log" || { echo "$ident: Shuttle did not execute exactly one test" >&2; exit 1; }
    rg -q 'test result: ok\. 1 passed; 0 failed;' "$log" || { echo "$ident: Shuttle test did not pass exactly once" >&2; exit 1; }
    jq -n \
        --arg id "$ident" \
        --arg test "$test_name" \
        --arg source "$source" \
        --arg log_sha256 "$(sha256sum "$log" | awk '{print $1}')" \
        '{id:$id,test:$test,source:$source,log_sha256:$log_sha256}' \
        >"$result_dir/$ident.json"
done

jq -s \
    --argjson iterations "$iterations" \
    --argjson pct_depth "$depth" \
    --argjson per_test_max_seconds "$seconds" \
    --arg registry_sha256 "$(sha256sum formal/concurrency-triangle.toml | awk '{print $1}')" \
    --arg proof_sha256 "$(sha256sum formal/shuttle-proof-kernel/src/lib.rs | awk '{print $1}')" \
    '{schema:"rustos-shuttle-evidence-v1",status:"passed",scheduler:"pct",iterations:$iterations,pct_depth:$pct_depth,per_test_max_seconds:$per_test_max_seconds,registry_sha256:$registry_sha256,proof_sha256:$proof_sha256,tests:.}' \
    "$result_dir"/*.json >"$artifact_dir/summary.json"
printf 'Shuttle PCT proof kernels passed\n'
