#!/usr/bin/env bash
# Validate the machine-readable end-to-end lifecycle contract graph.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

registry=formal/system-flows.tsv
models=formal/models.tsv
witnesses=formal/run-source-conformance.sh

test -f "$registry" || { echo "missing $registry" >&2; exit 1; }

declare -A seen_transition=()
declare -A seen_requirement=()
declare -A seen_hazard=()
declare -A flow_seen=()
declare -A flow_start=()
declare -A flow_terminal=()
declare -A from_state=()
declare -A continuing_target=()

rows=0
while IFS=$'\t' read -r flow transition requirement hazard severity owner from event to outcome \
    max_wait model source witness_package witness_test extra; do
    [[ -z "$flow" || "$flow" == \#* ]] && continue
    if [[ -n "${extra:-}" || -z "$witness_test" ]]; then
        echo "invalid system-flow column count for $flow/$transition" >&2
        exit 1
    fi
    [[ "$flow" =~ ^[a-z0-9-]+$ && "$transition" =~ ^[a-z0-9-]+$ ]] || {
        echo "invalid flow or transition id: $flow/$transition" >&2
        exit 1
    }
    [[ "$requirement" =~ ^REQ-[A-Z]+-[0-9]{3}$ ]] || {
        echo "invalid requirement id: $requirement" >&2
        exit 1
    }
    [[ "$hazard" =~ ^HAZ-[A-Z]+-[0-9]{3}$ ]] || {
        echo "invalid hazard id: $hazard" >&2
        exit 1
    }
    [[ "$severity" =~ ^(critical|high|medium|low)$ ]] || {
        echo "invalid flow severity: $severity" >&2
        exit 1
    }
    [[ "$owner" =~ ^[a-z0-9-]+$ ]] || { echo "invalid owner: $owner" >&2; exit 1; }
    [[ "$from" =~ ^(START|[a-z0-9-]+)$ && "$to" =~ ^[a-z0-9-]+$ ]] || {
        echo "invalid state in $flow/$transition" >&2
        exit 1
    }
    [[ "$event" =~ ^[a-z0-9-]+$ ]] || { echo "invalid event: $event" >&2; exit 1; }
    [[ "$outcome" =~ ^(continue|success|error|timeout|cancel|revoke|exit)$ ]] || {
        echo "invalid outcome in $flow/$transition: $outcome" >&2
        exit 1
    }
    [[ "$max_wait" =~ ^[0-9]+$ ]] || {
        echo "invalid max wait in $flow/$transition: $max_wait" >&2
        exit 1
    }
    if [[ "$outcome" == timeout && "$max_wait" == 0 ]]; then
        echo "timeout transition has no finite positive bound: $flow/$transition" >&2
        exit 1
    fi
    if [[ "$source" == *".ko"* || "$owner" == *".ko"* || "$event" == *".ko"* ]]; then
        echo "retired direct .ko route appears in common flow: $flow/$transition" >&2
        exit 1
    fi
    [[ -f "$source" ]] || { echo "missing flow source: $source" >&2; exit 1; }
    awk -F $'\t' -v wanted="$model" '$1 == wanted { found++ } END { exit(found == 1 ? 0 : 1) }' \
        "$models" || { echo "unregistered flow model: $model" >&2; exit 1; }
    grep -Fq -- "$model|$witness_package|$witness_test" "$witnesses" || {
        echo "missing source witness for $flow/$transition: $model -> $witness_package $witness_test" >&2
        exit 1
    }

    transition_key="$flow/$transition"
    [[ -z "${seen_transition[$transition_key]:-}" ]] || {
        echo "duplicate flow transition: $transition_key" >&2
        exit 1
    }
    [[ -z "${seen_requirement[$requirement]:-}" ]] || {
        echo "duplicate flow requirement: $requirement" >&2
        exit 1
    }
    [[ -z "${seen_hazard[$hazard]:-}" ]] || {
        echo "duplicate flow hazard: $hazard" >&2
        exit 1
    }
    seen_transition[$transition_key]=1
    seen_requirement[$requirement]=1
    seen_hazard[$hazard]=1
    flow_seen[$flow]=1
    from_state["$flow|$from"]=1
    [[ "$from" != START ]] || flow_start[$flow]=1
    if [[ "$outcome" == continue ]]; then
        continuing_target["$flow|$to"]="$transition_key"
    else
        flow_terminal[$flow]=1
    fi
    rows=$((rows + 1))
done < "$registry"

for flow in "${!flow_seen[@]}"; do
    [[ -n "${flow_start[$flow]:-}" ]] || { echo "flow has no START edge: $flow" >&2; exit 1; }
    [[ -n "${flow_terminal[$flow]:-}" ]] || { echo "flow has no terminal outcome: $flow" >&2; exit 1; }
done

for target in "${!continuing_target[@]}"; do
    [[ -n "${from_state[$target]:-}" ]] || {
        echo "continuing flow state has no outgoing transition: ${continuing_target[$target]} -> ${target#*|}" >&2
        exit 1
    }
done

printf 'system flow contracts passed: %s transitions %s flows\n' "$rows" "${#flow_seen[@]}"
