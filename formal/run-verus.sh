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
"$binary" formal/verus-proof-kernel/runtime_response.rs
