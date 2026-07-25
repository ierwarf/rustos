#!/usr/bin/env bash
# Prove registered unbounded mathematical lemmas with pinned TLAPS.
set -euo pipefail
repo_root="$(git rev-parse --show-toplevel)"
version="$(sed -n 's/^version=//p' "$repo_root/formal/tlaps.lock")"
install="${RUSTOS_TLAPS_HOME:-$HOME/.cache/rustos/tlaps/$version}"
tlapm="$install/bin/tlapm"
[[ -x "$tlapm" ]] || { echo "missing TLAPS; run: bash formal/setup-tlaps.sh" >&2; exit 2; }
artifact_dir="${TLAPS_ARTIFACT_DIR:-$repo_root/build/formal/tlaps}"
mkdir -p "$artifact_dir"

run_model() {
    local model="$1"
    local name="$2"
    local log="$artifact_dir/$name.log"
    if ! (cd "$artifact_dir" && "$tlapm" -I "$install/lib/tlaps" --threads 1 --timing \
        "$model") >"$log" 2>&1; then
        tail -n 80 "$log" >&2
        return 1
    fi
    grep -Eq 'All [1-9][0-9]* obligations? proved' "$log" || {
        echo "TLAPS produced no proved obligation for $name" >&2
        tail -n 80 "$log" >&2
        return 1
    }
}

run_model "$repo_root/formal/endpoint-publication/EndpointPublication.tla" \
    endpoint-publication
run_model "$repo_root/formal/userspace-wait-set/UserspaceWaitSet.tla" \
    userspace-wait-set
jq -n --arg version "$version" \
    '{schema:"rustos-tlaps-evidence-v1",status:"passed",tool:{name:"TLAPS",version:$version},models:2,minimum_obligations:2}' \
    >"$artifact_dir/summary.json"
printf 'TLAPS passed models=2 version=%s\n' "$version"
