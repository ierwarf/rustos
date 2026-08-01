#!/usr/bin/env bash
# Exact-tree, bounded evidence for iterative SMP debugging only.
#
# This deliberately does not replace the exhaustive PR seal used by release,
# FPS, or recovery gates. It keeps the edit/boot loop bounded while still
# forcing the high-risk SMP source mappings and executable models to agree.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
profile=smp-iteration
verification_dir="$repo_root/build/formal/verification-run"
mkdir -p "$verification_dir"
run_marker="$(mktemp "$verification_dir/$profile.started.XXXXXX")"
trap 'rm -f "$run_marker"' EXIT

bash formal/selftest.sh
cargo xtask formal-contracts check
bash formal/run-source-conformance.sh

FORMAL_SELFTEST_ALREADY_PASSED=1 \
    bash formal/run-all-tlc.sh --profile "$profile"

python3 formal/write-verification-run.py \
    --root "$repo_root" \
    --profile "$profile" \
    --not-before "$run_marker" \
    --output "$verification_dir/$profile.json"
