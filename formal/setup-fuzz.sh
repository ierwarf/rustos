#!/usr/bin/env bash
set -euo pipefail
repo_root="$(git rev-parse --show-toplevel)"
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
version="$(sed -n 's/^cargo_fuzz_version=//p' "$repo_root/formal/fuzz.lock")"
installed="$(cargo fuzz --version 2>/dev/null | awk '{print $2}' || true)"
if [[ "$installed" != "$version" ]]; then
    cargo install cargo-fuzz --version "$version" --locked
fi
