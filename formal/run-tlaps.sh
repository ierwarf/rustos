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
log="$artifact_dir/endpoint-publication.log"
if ! (cd "$artifact_dir" && "$tlapm" -I "$install/lib/tlaps" --threads 1 --timing \
    "$repo_root/formal/endpoint-publication/EndpointPublication.tla") >"$log" 2>&1; then
    tail -n 80 "$log" >&2
    exit 1
fi
grep -Eq 'All [1-9][0-9]* obligations? proved' "$log" || {
    echo "TLAPS produced no proved obligation" >&2
    tail -n 80 "$log" >&2
    exit 1
}
jq -n --arg version "$version" \
    '{schema:"rustos-tlaps-evidence-v1",status:"passed",tool:{name:"TLAPS",version:$version},model:"endpoint-publication/EndpointPublication",obligations:1}' \
    >"$artifact_dir/summary.json"
printf 'TLAPS passed model=endpoint-publication version=%s\n' "$version"
