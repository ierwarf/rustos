#!/usr/bin/env bash
# Detect UB in host-testable contract crates with the pinned nightly Miri.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
toolchain="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$repo_root/rust-toolchain.toml")"
command -v rustup >/dev/null 2>&1 || { echo "missing rustup" >&2; exit 2; }
rustup component list --toolchain "$toolchain" --installed | grep -q '^miri-' || {
    echo "missing Miri for $toolchain; run: bash formal/setup-miri.sh" >&2
    exit 2
}

cd "$repo_root"
export MIRIFLAGS="${MIRIFLAGS:--Zmiri-strict-provenance -Zmiri-symbolic-alignment-check}"
artifact_dir="${MIRI_ARTIFACT_DIR:-$repo_root/build/formal/miri}"
mkdir -p "$artifact_dir"
log="$artifact_dir/miri.log"
if ! {
    cargo "+$toolchain" miri test -q -p rustos-image-admission
    cargo "+$toolchain" miri test -q -p driver-domain-protocol --features std
    cargo "+$toolchain" miri test -q -p runtime-control \
        parse_exec_tokens_handles_quotes_and_placeholders
} >"$log" 2>&1; then
    tail -n 80 "$log" >&2
    exit 1
fi
jq -n --arg toolchain "$toolchain" --arg flags "$MIRIFLAGS" \
    '{schema:"rustos-miri-evidence-v1",status:"passed",toolchain:$toolchain,flags:$flags,packages:3}' \
    >"$artifact_dir/summary.json"
printf 'Miri passed packages=3\n'
