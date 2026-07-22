#!/usr/bin/env bash
# Install Miri for the repository-pinned nightly without changing the default toolchain.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
toolchain="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$repo_root/rust-toolchain.toml")"
[[ -n "$toolchain" ]] || { echo "cannot read pinned Rust toolchain" >&2; exit 2; }
rustup toolchain install "$toolchain" --profile minimal --component rust-src
rustup component add miri --toolchain "$toolchain"
cargo "+$toolchain" miri setup
