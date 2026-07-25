#!/usr/bin/env bash
# Prove that the wait-set invariants reject a deliberately reintroduced
# check/arm lost-wake bug. Mutation artifacts stay outside the source tree.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
source_spec="$repo_root/formal/userspace-wait-set/UserspaceWaitSet.tla"
mutation_root="$(mktemp -d "${TMPDIR:-/tmp}/rustos-formal-mutation.XXXXXX")"
trap 'rm -rf "$mutation_root"' EXIT
mutant="$mutation_root/UserspaceWaitSet.tla"
mutant_config="$mutation_root/UserspaceWaitSet.cfg"

cp "$source_spec" "$mutant"
cp "$repo_root/formal/userspace-wait-set/UserspaceWaitSet.cfg" "$mutant_config"
python3 - "$mutant" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
text = path.read_text()
old_ready = '''    /\\ waitState' = IF waitState \\in {"armed", "sleeping"}
                     THEN "woken" ELSE waitState
    /\\ UNCHANGED <<epoch, providerLive, observedGeneration, observedEpoch,
                   deadline, now, objectRefs, epollRefs, ingressBacklog>>

ConsumeReady =='''
new_ready = '''    /\\ waitState' = waitState
    /\\ UNCHANGED <<epoch, providerLive, observedGeneration, observedEpoch,
                   deadline, now, objectRefs, epollRefs, ingressBacklog>>

ConsumeReady =='''
old_recheck = '''                         ready \\/ generation # observedGeneration
                     THEN "woken" ELSE "sleeping"'''
new_recheck = '''                         FALSE
                     THEN "woken" ELSE "sleeping"'''
if text.count(old_ready) != 1 or text.count(old_recheck) != 1:
    raise SystemExit("mutation anchors drifted; update run-spec-mutations.sh")
path.write_text(text.replace(old_ready, new_ready).replace(old_recheck, new_recheck))
PY

set +e
FORMAL_MUTATION_MODE=1 \
TLA_SPEC_OVERRIDE="$mutant" \
TLA_CONFIG_OVERRIDE="$mutant_config" \
TLA_ARTIFACT_DIR="$mutation_root/evidence" \
bash "$repo_root/formal/run-tlc.sh" userspace-wait-set/UserspaceWaitSet \
    >"$mutation_root/run.log" 2>&1
result=$?
set -e
if [[ "$result" -eq 0 ]]; then
    echo "wait-set lost-wake mutant escaped all registered invariants" >&2
    exit 1
fi
rg -q 'SleepingRequiresStableRecheck|Invariant.*violated|Error: Invariant' \
    "$mutation_root/run.log" "$mutation_root/evidence" || {
    echo "mutant failed for an unrelated reason" >&2
    tail -n 80 "$mutation_root/run.log" >&2
    exit 1
}
summary_dir="$repo_root/build/formal/mutations"
mkdir -p "$summary_dir"
jq -n \
    --arg source_sha256 "$(sha256sum "$source_spec" | awk '{print $1}')" \
    --arg mutant_sha256 "$(sha256sum "$mutant" | awk '{print $1}')" \
    '{schema:"rustos-formal-mutation-evidence-v1",status:"passed",model:"userspace-wait-set/UserspaceWaitSet",mutation:"remove-check-arm-readiness-recheck",source_sha256:$source_sha256,mutant_sha256:$mutant_sha256,expected_result:"invariant-rejected"}' \
    >"$summary_dir/summary.json"
printf 'Formal mutation gate passed: lost-wake mutant rejected\n'
