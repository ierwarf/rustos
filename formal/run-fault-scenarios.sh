#!/usr/bin/env bash
# Prove that configured fault rules name live boundaries and make no phantom
# runtime-evidence claim.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
artifact_dir="$repo_root/build/formal/fault-scenarios"
python3 "$repo_root/formal/check-fault-scenarios.py" \
    --root "$repo_root" \
    --summary "$artifact_dir/summary.json"

declare -A witnesses=()
while IFS=$'\t' read -r point _severity _owner _source _expected _evidence _witness_source witness; do
    [[ -n "$point" && "$point" != \#* ]] || continue
    witnesses["$witness"]=1
done <"$repo_root/formal/fault-scenarios.tsv"

for witness in "${!witnesses[@]}"; do
    package="${witness%%:*}"
    test_name="${witness#*:}"
    artifact_name="${package}-${test_name}"
    list_log="$artifact_dir/${artifact_name}.list.log"
    test_log="$artifact_dir/${artifact_name}.test.log"
    cargo test -q -p "$package" "$test_name" -- --list >"$list_log" 2>&1
    listed="$(grep -Ec "(^|::)${test_name}: test$" "$list_log" || true)"
    if [[ "$listed" != 1 ]]; then
        echo "fault witness must resolve to exactly one test: $witness (found $listed)" >&2
        tail -n 40 "$list_log" >&2
        exit 1
    fi
    if ! cargo test -q -p "$package" "$test_name" >"$test_log" 2>&1; then
        tail -n 80 "$test_log" >&2
        exit 1
    fi
done

printf 'fault source witnesses passed count=%s\n' "${#witnesses[@]}"
