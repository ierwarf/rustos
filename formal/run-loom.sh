#!/usr/bin/env bash
# Exhaustively enumerate the small synchronization proof kernels.
set -euo pipefail
repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
artifact_dir="${LOOM_ARTIFACT_DIR:-$repo_root/build/formal/loom}"
mkdir -p "$artifact_dir"
branches="${LOOM_MAX_BRANCHES:-200}"
[[ "$branches" =~ ^[1-9][0-9]*$ ]] || { echo "LOOM_MAX_BRANCHES must be positive" >&2; exit 2; }
proof_source_root="formal/loom-proof-kernel/src"
loom_test_list="$(
    cargo test -q --manifest-path formal/loom-proof-kernel/Cargo.toml -- --list
)"
declare -a production_inputs=()
while IFS=$'\t' read -r proof_test production_source production_symbol invariant; do
    [[ -n "$proof_test" && "$proof_test" != \#* ]] || continue
    source_path="$repo_root/$production_source"
    [[ -f "$source_path" ]] || { echo "missing Loom production source: $production_source" >&2; exit 1; }
    proof_source_matches="$(
        { rg -n --glob '*.rs' "fn ${proof_test}\\(" "$proof_source_root" || true; } \
            | wc -l
    )"
    [[ "$proof_source_matches" -eq 1 ]] \
        || { echo "Loom proof test must have one source definition: $proof_test" >&2; exit 1; }
    rg -Fxq "tests::${proof_test}: test" <<<"$loom_test_list" \
        || { echo "Loom proof test is not compiled at its exact FQN: $proof_test" >&2; exit 1; }
    symbol_leaf="${production_symbol##*::}"
    rg -q "fn ${symbol_leaf}\\b" "$source_path" \
        || { echo "missing Loom production symbol: $production_source::$production_symbol" >&2; exit 1; }
    if [[ "$production_symbol" == *::* ]]; then
        symbol_owner="${production_symbol%%::*}"
        rg -q "(struct|enum|trait|impl[^\\n]*) ${symbol_owner}\\b" "$source_path" \
            || { echo "missing Loom production owner: $production_source::$symbol_owner" >&2; exit 1; }
    fi
    [[ -n "$invariant" ]] || { echo "empty Loom invariant for $proof_test" >&2; exit 1; }
    production_inputs+=("$production_source")
done <formal/concurrency-witnesses.tsv
if ! LOOM_MAX_BRANCHES="$branches" \
    cargo test -q --manifest-path formal/loom-proof-kernel/Cargo.toml \
    >"$artifact_dir/loom.log" 2>&1; then
    tail -n 80 "$artifact_dir/loom.log" >&2
    exit 1
fi
proof_count="${#production_inputs[@]}"
mapfile -t production_inputs < <(printf '%s\n' "${production_inputs[@]}" | sort -u)
proof_sha256="$(
    rg --files "$proof_source_root" -g '*.rs' \
        | sort \
        | xargs sha256sum \
        | sha256sum \
        | awk '{print $1}'
)"
production_hashes="$(
    for source in "${production_inputs[@]}"; do
        jq -n --arg path "$source" --arg sha256 "$(sha256sum "$source" | awk '{print $1}')" \
            '{path:$path,sha256:$sha256}'
    done | jq -s .
)"
jq -n \
    --argjson branches "$branches" \
    --argjson proof_count "$proof_count" \
    --arg registry_sha256 "$(sha256sum formal/concurrency-witnesses.tsv | awk '{print $1}')" \
    --arg proof_sha256 "$proof_sha256" \
    --argjson production_inputs "$production_hashes" \
    '{schema:"rustos-loom-evidence-v2",status:"passed",proof_kernels:$proof_count,max_branches:$branches,inputs:{registry_sha256:$registry_sha256,proof_sha256:$proof_sha256,production:$production_inputs}}' \
    >"$artifact_dir/summary.json"
printf 'Loom proof kernels passed\n'
