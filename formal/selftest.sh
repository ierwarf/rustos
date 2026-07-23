#!/usr/bin/env bash
# Verify that every formal model is registered once with an executable contract.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

registry=formal/models.tsv
test -f "$registry" || { echo "missing $registry" >&2; exit 1; }

registered="$({
    while IFS=$'\t' read -r model class deadlock reason pr_timeout nightly_timeout nightly_mode apalache tlaps trace extra; do
        [[ -z "$model" || "$model" == \#* ]] && continue
        if [[ -n "${extra:-}" || -z "$trace" ]]; then
            echo "invalid registry column count for $model" >&2
            exit 1
        fi
        [[ "$class" == safety || "$class" == temporal ]] || { echo "invalid class for $model: $class" >&2; exit 1; }
        [[ "$deadlock" == check || "$deadlock" == intentional-terminal ]] || { echo "invalid deadlock policy for $model: $deadlock" >&2; exit 1; }
        [[ "$deadlock" != intentional-terminal || -n "$reason" ]] || { echo "missing terminal rationale for $model" >&2; exit 1; }
        [[ "$pr_timeout" =~ ^[1-9][0-9]*$ && "$nightly_timeout" =~ ^[1-9][0-9]*$ ]] || { echo "invalid timeout for $model" >&2; exit 1; }
        [[ "$nightly_mode" == exhaustive || "$nightly_mode" == exhaustive+simulate ]] || { echo "invalid nightly mode for $model: $nightly_mode" >&2; exit 1; }
        for flag in "$apalache" "$tlaps" "$trace"; do
            [[ "$flag" == yes || "$flag" == no ]] || { echo "invalid pilot flag for $model: $flag" >&2; exit 1; }
        done

        spec="formal/$model.tla"
        cfg="formal/$model.cfg"
        [[ -f "$spec" && -f "$cfg" ]] || { echo "missing TLA/CFG pair for $model" >&2; exit 1; }
        module="$(basename "$model")"
        head -n 1 "$spec" | grep -Eq "MODULE[[:space:]]+$module[[:space:]]" || { echo "module name mismatch for $model" >&2; exit 1; }
        rg -q '^(INVARIANT|INVARIANTS)([[:space:]]|$)' "$cfg" || { echo "no invariant configured for $model" >&2; exit 1; }
        has_property=no
        rg -q '^PROPERTY|^PROPERTIES' "$cfg" && has_property=yes
        if [[ "$class" == temporal && "$has_property" != yes ]]; then
            echo "temporal model has no property: $model" >&2
            exit 1
        fi
        if [[ "$class" == temporal ]] && ! rg -q '^SPECIFICATION[[:space:]]+Spec([[:space:]]|$)' "$cfg"; then
            echo "temporal model bypasses its Spec fairness/behavior formula: $model" >&2
            exit 1
        fi
        if [[ "$class" == safety && "$has_property" == yes ]]; then
            echo "safety model config contains an unregistered temporal property: $model" >&2
            exit 1
        fi
        model_slug="$(dirname "$model")"
        rg -q "$model_slug|$module" formal/CONFORMANCE.md || { echo "missing conformance mapping for $model" >&2; exit 1; }
        rg -q "$model_slug|$module" formal/COVERAGE.md || { echo "missing coverage mapping for $model" >&2; exit 1; }
        rg -q "$model_slug|$module" formal/README.md || { echo "missing README mapping for $model" >&2; exit 1; }
        if [[ "$apalache" == yes ]]; then
            [[ -f "formal/apalache-pilots/${module}Pilot.tla" ]] || { echo "missing Apalache pilot for $model" >&2; exit 1; }
        fi
        if [[ "$tlaps" == yes ]]; then
            rg -q '^THEOREM ' "$spec" || { echo "missing TLAPS theorem for $model" >&2; exit 1; }
        fi
        if [[ "$trace" == yes ]]; then
            rg -q "model.*$model|$model" formal/check-runtime-trace.py || { echo "missing runtime trace checker for $model" >&2; exit 1; }
        fi
        printf '%s\n' "$model"
    done < "$registry"
} | sort)"

if [[ -n "$(printf '%s\n' "$registered" | uniq -d)" ]]; then
    echo "duplicate formal model registry entry" >&2
    printf '%s\n' "$registered" | uniq -d >&2
    exit 1
fi

present="$(find formal -mindepth 2 -maxdepth 2 -name '*.cfg' -printf '%P\n' | sed 's/\.cfg$//' | sort)"
if ! diff -u <(printf '%s\n' "$present") <(printf '%s\n' "$registered"); then
    echo "formal model registry does not match TLA sources" >&2
    exit 1
fi

present_specs="$(find formal -mindepth 2 -maxdepth 2 -name '*.tla' \
    ! -path 'formal/apalache-pilots/*' -printf '%P\n' | sed 's/\.tla$//' | sort)"
if ! diff -u <(printf '%s\n' "$present_specs") <(printf '%s\n' "$registered"); then
    echo "formal model registry does not match primary TLA specifications" >&2
    exit 1
fi

while IFS= read -r cfg; do
    tla="${cfg%.cfg}.tla"
    [[ -f "$tla" ]] || { echo "orphan formal config: $cfg" >&2; exit 1; }
done < <(find formal -mindepth 2 -maxdepth 2 -name '*.cfg' | sort)

for lock in formal/tla2tools.lock formal/kani.lock formal/verus.lock formal/apalache.lock formal/tlaps.lock; do
    [[ -s "$lock" ]] || { echo "missing tool lock: $lock" >&2; exit 1; }
done
for script in formal/run-{all-tlc,tlc,tlc-simulate,kani,verus,runtime-traces,source-conformance,miri,loom,fuzz-smoke,apalache,tlaps}.sh; do
    [[ -x "$script" ]] || { echo "formal runner is not executable: $script" >&2; exit 1; }
done
[[ -x formal/check-system-flows.sh ]] || {
    echo "system-flow contract checker is not executable" >&2
    exit 1
}
formal/check-system-flows.sh
[[ -x formal/check-zero-trust-ingress.sh ]] || {
    echo "zero-trust ingress contract checker is not executable" >&2
    exit 1
}
formal/check-zero-trust-ingress.sh
[[ -x formal/check-zero-trust-subsystems.sh ]] || {
    echo "zero-trust subsystem contract checker is not executable" >&2
    exit 1
}
formal/check-zero-trust-subsystems.sh
rg -q 'run-source-conformance\.sh' formal/verify-all.sh || {
    echo "formal PR gate omits source conformance" >&2
    exit 1
}

printf 'formal selftest passed: %s registered models\n' "$(printf '%s\n' "$registered" | wc -l)"
