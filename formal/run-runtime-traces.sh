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
    topology="$(python3 - "$artifact_dir/kvm-p0.jsonl" <<'PY'
import json
import sys
for line in open(sys.argv[1], encoding="utf-8"):
    if line.strip():
        print(json.loads(line)["topology"])
        break
else:
    raise SystemExit("KVM trace is empty")
PY
)"
    set +e
    python3 formal/check-kvm-runtime-trace.py "$artifact_dir/kvm-p0.jsonl" \
        --root "$repo_root" \
        --registry "$repo_root/formal/product-scenarios.tsv" \
        --topology "$topology" \
        --summary "$artifact_dir/kvm-p0-summary.json" \
        --classify-stale
    kvm_trace_status=$?
    set -e
    if [[ "$kvm_trace_status" -eq 3 && "${FORMAL_REQUIRE_KVM_TRACE:-0}" != 1 ]]; then
        echo "optional KVM trace is stale and was not admitted as current evidence"
    elif [[ "$kvm_trace_status" -ne 0 ]]; then
        exit "$kvm_trace_status"
    fi
elif [[ "${FORMAL_REQUIRE_KVM_TRACE:-0}" == 1 ]]; then
    echo "required KVM P0 runtime trace is missing" >&2
    exit 1
fi
