#!/usr/bin/env bash
# Generate concrete source traces and replay them against registered TLA+ pilots.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
artifact_dir="${FORMAL_TRACE_ARTIFACT_DIR:-$repo_root/build/formal/runtime-traces}"
mkdir -p "$artifact_dir"
trace="$artifact_dir/runtime-control-rpc.jsonl"

RUSTOS_FORMAL_TRACE_OUT="$trace" \
    cargo test -q -p runtime-control tests::emit_runtime_control_rpc_formal_trace -- --exact
python3 formal/check-runtime-trace.py "$trace" \
    --summary "$artifact_dir/runtime-control-rpc-summary.json"
if [[ -f "$artifact_dir/kvm-p0.jsonl" ]]; then
    python3 formal/check-kvm-runtime-trace.py "$artifact_dir/kvm-p0.jsonl" \
        --summary "$artifact_dir/kvm-p0-summary.json"
elif [[ "${FORMAL_REQUIRE_KVM_TRACE:-0}" == 1 ]]; then
    echo "required KVM P0 runtime trace is missing" >&2
    exit 1
fi
