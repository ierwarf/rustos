#!/usr/bin/env bash
# Tiered formal gate. PR is exhaustive and deterministic; nightly adds bug-finding lanes.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
profile="pr"
if [[ "${1:-}" == "--profile" ]]; then
    profile="${2:-}"
    shift 2
fi
[[ "$#" -eq 0 ]] || { echo "usage: $0 [--profile pr|nightly]" >&2; exit 2; }
case "$profile" in
    pr|nightly) ;;
    *) echo "invalid formal profile: $profile" >&2; exit 2 ;;
esac

verification_dir="$repo_root/build/formal/verification-run"
mkdir -p "$verification_dir"
run_marker="$(mktemp "$verification_dir/$profile.started.XXXXXX")"
trap 'rm -f "$run_marker"' EXIT

bash formal/selftest.sh
bash formal/run-source-conformance.sh
bash formal/run-all-tlc.sh --profile "$profile"
bash formal/run-spec-mutations.sh
bash formal/run-fault-scenarios.sh
bash formal/run-abi-differential.sh
bash formal/run-recovery-scenarios.sh
bash formal/run-implementation-mutations.sh
bash formal/run-kani.sh
bash formal/run-verus.sh
bash formal/run-runtime-traces.sh
if [[ "$profile" == nightly ]]; then
    bash formal/run-sanitizers.sh --profile=all
    bash formal/run-miri.sh
    bash formal/run-loom.sh
    bash formal/run-apalache.sh
    bash formal/run-tlaps.sh
    bash formal/run-fuzz-smoke.sh
fi
python3 formal/write-verification-run.py \
    --root "$repo_root" \
    --profile "$profile" \
    --not-before "$run_marker" \
    --output "$repo_root/build/formal/verification-run/$profile.json"
