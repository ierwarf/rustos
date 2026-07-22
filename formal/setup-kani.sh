#!/usr/bin/env bash
# Install the pinned Kani verifier outside the worktree.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$cargo_home/bin:$PATH"
lock="$repo_root/formal/kani.lock"
version="$(sed -n 's/^version=//p' "$lock" | head -n 1)"

if [[ -z "$version" ]]; then
    echo "invalid $lock" >&2
    exit 2
fi

installed=""
if command -v cargo-kani >/dev/null 2>&1; then
    installed="$(cargo kani --version | awk 'NR == 1 { print $2 }')"
fi

if [[ "$installed" != "$version" ]]; then
    cargo install --locked kani-verifier --version "$version"
fi

cargo kani setup
