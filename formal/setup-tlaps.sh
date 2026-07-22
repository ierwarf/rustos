#!/usr/bin/env bash
set -euo pipefail
repo_root="$(git rev-parse --show-toplevel)"
lock="$repo_root/formal/tlaps.lock"
version="$(sed -n 's/^version=//p' "$lock")"
archive="$(sed -n 's/^archive=//p' "$lock")"
sha256="$(sed -n 's/^sha256=//p' "$lock")"
url="$(sed -n 's/^url=//p' "$lock")"
cache="${RUSTOS_TLAPS_CACHE:-$HOME/.cache/rustos/tlaps}"
download="$cache/download/$archive"
install="$cache/$version"
mkdir -p "$cache/download" "$install"
if [[ ! -f "$download" ]] || ! printf '%s  %s\n' "$sha256" "$download" | sha256sum -c - >/dev/null 2>&1; then
    curl -fL --retry 3 "$url" -o "$download"
fi
printf '%s  %s\n' "$sha256" "$download" | sha256sum -c -
chmod +x "$download"
if [[ ! -x "$install/bin/tlapm" ]]; then
    "$download" -d "$install"
fi
[[ "$($install/bin/tlapm --version)" == "$version" ]] || { echo "TLAPS version mismatch" >&2; exit 1; }
