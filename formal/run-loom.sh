#!/usr/bin/env bash
# Exhaustively enumerate the small synchronization proof kernels.
set -euo pipefail
repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
artifact_dir="${LOOM_ARTIFACT_DIR:-$repo_root/build/formal/loom}"
mkdir -p "$artifact_dir"
branches="${LOOM_MAX_BRANCHES:-200}"
[[ "$branches" =~ ^[1-9][0-9]*$ ]] || { echo "LOOM_MAX_BRANCHES must be positive" >&2; exit 2; }
declare -a production_inputs=()
while IFS=$'\t' read -r proof_test production_source production_symbol invariant; do
    [[ -n "$proof_test" && "$proof_test" != \#* ]] || continue
    source_path="$repo_root/$production_source"
    [[ -f "$source_path" ]] || { echo "missing Loom production source: $production_source" >&2; exit 1; }
    rg -q "fn ${proof_test}\\(" formal/loom-proof-kernel/src/lib.rs \
        || { echo "missing Loom proof test: $proof_test" >&2; exit 1; }
    rg -q "fn ${production_symbol}\\b" "$source_path" \
        || { echo "missing Loom production symbol: $production_source::$production_symbol" >&2; exit 1; }
    [[ -n "$invariant" ]] || { echo "empty Loom invariant for $proof_test" >&2; exit 1; }
    production_inputs+=("$production_source")
done <formal/concurrency-witnesses.tsv
if ! LOOM_MAX_BRANCHES="$branches" \
    cargo test -q --manifest-path formal/loom-proof-kernel/Cargo.toml \
    >"$artifact_dir/loom.log" 2>&1; then
    tail -n 80 "$artifact_dir/loom.log" >&2
    exit 1
fi
mapfile -t production_inputs < <(printf '%s\n' "${production_inputs[@]}" | sort -u)
production_hashes="$(
    for source in "${production_inputs[@]}"; do
        jq -n --arg path "$source" --arg sha256 "$(sha256sum "$source" | awk '{print $1}')" \
            '{path:$path,sha256:$sha256}'
    done | jq -s .
)"
jq -n \
    --argjson branches "$branches" \
    --arg registry_sha256 "$(sha256sum formal/concurrency-witnesses.tsv | awk '{print $1}')" \
    --arg proof_sha256 "$(sha256sum formal/loom-proof-kernel/src/lib.rs | awk '{print $1}')" \
    --argjson production_inputs "$production_hashes" \
    '{schema:"rustos-loom-evidence-v2",status:"passed",proof_kernels:3,max_branches:$branches,inputs:{registry_sha256:$registry_sha256,proof_sha256:$proof_sha256,production:$production_inputs}}' \
    >"$artifact_dir/summary.json"
printf 'Loom proof kernels passed\n'
