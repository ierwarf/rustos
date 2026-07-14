#!/usr/bin/env bash
# Run bounded Rust implementation proofs with no unsound analysis flags.
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
if ! command -v cargo-kani >/dev/null 2>&1; then
    echo "missing cargo-kani; run: bash formal/setup-kani.sh" >&2
    exit 2
fi

installed="$(cargo kani --version | awk 'NR == 1 { print $2 }')"
if [[ "$installed" != "$version" ]]; then
    echo "cargo-kani version $installed does not match pinned $version" >&2
    exit 2
fi

cd "$repo_root"
cargo kani -p runtime-control --output-format terse -Z unstable-options --run-sanity-checks
cargo kani -p driver-domain-protocol --output-format terse -Z unstable-options --run-sanity-checks
