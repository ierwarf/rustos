#!/usr/bin/env bash
# Verify the pinned, unbounded proof-kernel theorems.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
lock="$repo_root/formal/verus.lock"

read_lock_value() {
    local key="$1"
    sed -n "s/^$key=//p" "$lock" | head -n 1
}

version="$(read_lock_value version)"
cache_root="${VERUS_CACHE_DIR:-$HOME/.cache/rustos/verus}"
binary="$cache_root/$version/verus-x86-linux/verus"
if [[ -z "$version" || ! -x "$binary" ]]; then
    echo "missing pinned Verus; run: bash formal/setup-verus.sh" >&2
    exit 2
fi

installed="$($binary --version | sed -n 's/^  Version: //p' | head -n 1)"
if [[ "$installed" != "$version" ]]; then
    echo "Verus version $installed does not match pinned $version" >&2
    exit 2
fi

cd "$repo_root"
artifact_dir="${VERUS_ARTIFACT_DIR:-$repo_root/build/formal/verus}"
mkdir -p "$artifact_dir"
if ! "$binary" formal/verus-proof-kernel/runtime_response.rs >"$artifact_dir/verus.log" 2>&1; then
    tail -n 80 "$artifact_dir/verus.log" >&2
    exit 1
fi
jq -n --arg version "$version" \
    '{schema:"rustos-verus-evidence-v1",status:"passed",tool:{name:"Verus",version:$version},proof_file:"formal/verus-proof-kernel/runtime_response.rs"}' \
    >"$artifact_dir/summary.json"
printf 'Verus proof kernel passed version=%s\n' "$version"
