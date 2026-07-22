#!/usr/bin/env bash
# Exhaustively enumerate the small synchronization proof kernels.
set -euo pipefail
repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
artifact_dir="${LOOM_ARTIFACT_DIR:-$repo_root/build/formal/loom}"
mkdir -p "$artifact_dir"
branches="${LOOM_MAX_BRANCHES:-200}"
[[ "$branches" =~ ^[1-9][0-9]*$ ]] || { echo "LOOM_MAX_BRANCHES must be positive" >&2; exit 2; }
if ! LOOM_MAX_BRANCHES="$branches" \
    cargo test -q --manifest-path formal/loom-proof-kernel/Cargo.toml \
    >"$artifact_dir/loom.log" 2>&1; then
    tail -n 80 "$artifact_dir/loom.log" >&2
    exit 1
fi
jq -n --argjson branches "$branches" \
    '{schema:"rustos-loom-evidence-v1",status:"passed",proof_kernels:1,max_branches:$branches}' \
    >"$artifact_dir/summary.json"
printf 'Loom proof kernels passed\n'
