#!/usr/bin/env bash
# Run one pinned TLC model. The jar and TLC state stay outside the worktree.
set -eo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: bash formal/run-tlc.sh <model/path-without-extension>" >&2
    exit 2
fi

repo_root="$(git rev-parse --show-toplevel)"
formal_dir="$repo_root/formal"
model="$1"
spec="$formal_dir/$model.tla"
config="$formal_dir/$model.cfg"
lock="$formal_dir/tla2tools.lock"

if [[ ! -f "$spec" || ! -f "$config" ]]; then
    echo "missing TLA+ model or configuration for $model" >&2
    exit 2
fi

read_lock_value() {
    local key="$1"
    sed -n "s/^$key=//p" "$lock" | head -n 1
}

version="$(read_lock_value version)"
url="$(read_lock_value url)"
sha256="$(read_lock_value sha256)"
if [[ -z "$version" || -z "$url" || ! "$sha256" =~ ^[0-9a-f]{64}$ ]]; then
    echo "invalid $lock" >&2
    exit 2
fi

cache_root="$HOME/.cache/rustos/tla"
if [[ -n "$TLA_CACHE_DIR" ]]; then
    cache_root="$TLA_CACHE_DIR"
fi
jar="$cache_root/tla2tools-$version.jar"
mkdir -p "$cache_root"

verify_jar() {
    printf '%s  %s\n' "$sha256" "$1" | sha256sum --check --status
}

if [[ ! -f "$jar" ]] || ! verify_jar "$jar"; then
    tmp_jar="$(mktemp "$cache_root/tla2tools-$version.XXXXXX")"
    trap 'rm -f "$tmp_jar"' EXIT
    curl --fail --location --proto '=https' --tlsv1.2 --retry 3 \
        --output "$tmp_jar" "$url"
    if ! verify_jar "$tmp_jar"; then
        echo "downloaded TLC jar did not match $lock" >&2
        exit 1
    fi
    mv "$tmp_jar" "$jar"
fi

tmp_root=/tmp
if [[ -n "$TMPDIR" ]]; then
    tmp_root="$TMPDIR"
fi
model_dir="$(dirname "$spec")"
state_dir="$(mktemp -d "$tmp_root/rustos-tlc.XXXXXX")"
trap 'rm -rf "$state_dir"' EXIT

(
    cd "$model_dir"
    java -XX:+UseParallelGC -jar "$jar" \
        -workers 1 \
        -fp 0 \
        -seed 1 \
        -deadlock \
        -metadir "$state_dir" \
        -config "$config" \
        "$(basename "$spec")"
)
