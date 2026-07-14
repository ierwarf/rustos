#!/usr/bin/env bash
# Install the pinned Verus release outside the worktree and verify its archive.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
lock="$repo_root/formal/verus.lock"

read_lock_value() {
    local key="$1"
    sed -n "s/^$key=//p" "$lock" | head -n 1
}

version="$(read_lock_value version)"
toolchain="$(read_lock_value toolchain)"
url="$(read_lock_value url)"
sha256="$(read_lock_value sha256)"
if [[ -z "$version" || -z "$toolchain" || -z "$url" || ! "$sha256" =~ ^[0-9a-f]{64}$ ]]; then
    echo "invalid $lock" >&2
    exit 2
fi

cache_root="${VERUS_CACHE_DIR:-$HOME/.cache/rustos/verus}"
release_dir="$cache_root/$version"
archive="$release_dir/verus.zip"
bundle="$release_dir/verus-x86-linux"
binary="$bundle/verus"
mkdir -p "$release_dir"

verify_archive() {
    printf '%s  %s\n' "$sha256" "$archive" | sha256sum --check --status
}

if [[ ! -f "$archive" ]] || ! verify_archive; then
    tmp_archive="$(mktemp "$release_dir/verus.XXXXXX")"
    trap 'rm -f "$tmp_archive"' EXIT
    curl --fail --location --proto '=https' --tlsv1.2 --retry 3 --output "$tmp_archive" "$url"
    printf '%s  %s\n' "$sha256" "$tmp_archive" | sha256sum --check --status
    mv "$tmp_archive" "$archive"
fi

if [[ ! -x "$binary" ]]; then
    unzip -n -q "$archive" -d "$release_dir"
fi
if [[ ! -x "$binary" ]]; then
    echo "Verus bundle did not contain $binary" >&2
    exit 1
fi

rustup toolchain install "$toolchain" --profile minimal
