#!/usr/bin/env bash
set -euo pipefail
repo_root="$(git rev-parse --show-toplevel)"
lock="$repo_root/formal/apalache.lock"
version="$(sed -n 's/^version=//p' "$lock")"
archive="$(sed -n 's/^archive=//p' "$lock")"
sha256="$(sed -n 's/^sha256=//p' "$lock")"
url="$(sed -n 's/^url=//p' "$lock")"
cache="${RUSTOS_APALACHE_CACHE:-$HOME/.cache/rustos/apalache}"
download="$cache/download/$archive"
install="$cache/$version"
mkdir -p "$cache/download" "$install"
if [[ ! -f "$download" ]] || ! printf '%s  %s\n' "$sha256" "$download" | sha256sum -c - >/dev/null 2>&1; then
    curl -fL --retry 3 "$url" -o "$download"
fi
printf '%s  %s\n' "$sha256" "$download" | sha256sum -c -
if [[ ! -x "$install/bin/apalache-mc" ]]; then
    tar -xzf "$download" --strip-components=1 -C "$install"
fi
[[ "$($install/bin/apalache-mc version)" == "$version" ]] || { echo "Apalache version mismatch" >&2; exit 1; }
