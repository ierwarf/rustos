#!/usr/bin/env bash
# Run one pinned TLC model and retain machine-readable evidence under build/.
set -euo pipefail

profile=pr
if [[ $# -eq 3 && "$1" == --profile ]]; then
    profile="$2"
    model="$3"
elif [[ $# -eq 1 ]]; then
    model="$1"
else
    echo "usage: bash formal/run-tlc.sh [--profile pr|smp-iteration|nightly] <model/path-without-extension>" >&2
    exit 2
fi
[[ "$profile" == pr || "$profile" == smp-iteration || "$profile" == nightly ]] || {
    echo "invalid TLC profile: $profile" >&2
    exit 2
}

repo_root="$(git rev-parse --show-toplevel)"
formal_dir="$repo_root/formal"
registry="$formal_dir/models.tsv"
row="$(awk -F '\t' -v wanted="$model" '$1 == wanted { print; found++ } END { if (found != 1) exit 1 }' "$registry")" || {
    echo "model is not uniquely registered: $model" >&2
    exit 2
}
IFS=$'\t' read -r _model _class deadlock_policy _reason pr_timeout nightly_timeout _nightly_mode _apalache _tlaps _trace <<< "$row"

if [[ -n "${TLA_SPEC_OVERRIDE:-}${TLA_CONFIG_OVERRIDE:-}" &&
      "${FORMAL_MUTATION_MODE:-0}" != 1 ]]; then
    echo "TLA overrides are restricted to FORMAL_MUTATION_MODE=1" >&2
    exit 2
fi
spec="${TLA_SPEC_OVERRIDE:-$formal_dir/$model.tla}"
config="${TLA_CONFIG_OVERRIDE:-$formal_dir/$model.cfg}"
lock="$formal_dir/tla2tools.lock"
[[ -f "$spec" && -f "$config" ]] || { echo "missing TLA+ model or configuration for $model" >&2; exit 2; }

read_lock_value() { sed -n "s/^$1=//p" "$lock" | head -n 1; }
version="$(read_lock_value version)"
url="$(read_lock_value url)"
sha256="$(read_lock_value sha256)"
if [[ -z "$version" || -z "$url" || ! "$sha256" =~ ^[0-9a-f]{64}$ ]]; then
    echo "invalid $lock" >&2
    exit 2
fi

cache_root="${TLA_CACHE_DIR:-$HOME/.cache/rustos/tla}"
jar="$cache_root/tla2tools-$version.jar"
mkdir -p "$cache_root"
verify_jar() { printf '%s  %s\n' "$sha256" "$1" | sha256sum --check --status; }
if [[ ! -f "$jar" ]] || ! verify_jar "$jar"; then
    tmp_jar="$(mktemp "$cache_root/tla2tools-$version.XXXXXX")"
    trap 'rm -f "$tmp_jar"' EXIT
    curl --fail --location --proto '=https' --tlsv1.2 --retry 3 --output "$tmp_jar" "$url"
    verify_jar "$tmp_jar" || { echo "downloaded TLC jar did not match $lock" >&2; exit 1; }
    mv "$tmp_jar" "$jar"
fi

if [[ "$profile" == pr || "$profile" == smp-iteration ]]; then
    timeout_seconds="$pr_timeout"
    if [[ "$profile" == smp-iteration && "$timeout_seconds" -gt 30 ]]; then
        timeout_seconds=30
    fi
    workers="${TLC_WORKERS:-auto}"
    fingerprint="${TLC_FP:-0}"
    seed="${TLC_SEED:-1}"
else
    timeout_seconds="$nightly_timeout"
    workers="${TLC_WORKERS:-1}"
    fingerprint="${TLC_FP:-127}"
    seed="${TLC_SEED:-20260721}"
fi
[[ "$workers" == auto || "$workers" =~ ^[1-9][0-9]*$ ]] || { echo "TLC_WORKERS must be auto or a positive integer" >&2; exit 2; }
[[ "$fingerprint" =~ ^([0-9]|[1-9][0-9]|1[0-2][0-9]|130)$ ]] || { echo "TLC_FP must be 0..130" >&2; exit 2; }
[[ "$seed" =~ ^[0-9]+$ ]] || { echo "TLC_SEED must be a non-negative integer" >&2; exit 2; }

artifact_dir="${TLA_ARTIFACT_DIR:-$repo_root/build/formal/tlc/$profile/${model//\//__}}"
mkdir -p "$artifact_dir"
log="$artifact_dir/tlc.log"
summary="$artifact_dir/summary.json"
state_dir="$(mktemp -d "${TMPDIR:-/tmp}/rustos-tlc.XXXXXX")"
trap 'rm -rf "$state_dir"' EXIT

deadlock_args=()
if [[ "$deadlock_policy" == intentional-terminal ]]; then
    deadlock_args=(-deadlock)
fi

set +e
(
    cd "$artifact_dir"
    timeout --signal=TERM --kill-after=10 "$timeout_seconds" \
        java -XX:+UseParallelGC -jar "$jar" \
        -workers "$workers" \
        -fp "$fingerprint" \
        -seed "$seed" \
        -coverage 1 \
        "${deadlock_args[@]}" \
        -generateSpecTE nomonolith \
        -metadir "$state_dir" \
        -config "$config" \
        "$spec"
) >"$log" 2>&1
result=$?
set -e

if [[ "$result" -ne 0 ]]; then
    # TLC writes SpecTE beside the input module even when its process cwd and
    # metadir point at build/.  Preserve the counterexample as evidence without
    # leaving generated modules in the source registry.
    generated_spec_dir="$(dirname "$spec")"
    [[ ! -f "$generated_spec_dir/SpecTE.tla" ]] || \
        mv "$generated_spec_dir/SpecTE.tla" "$artifact_dir/counterexample-SpecTE.tla"
    [[ ! -f "$generated_spec_dir/SpecTE.cfg" ]] || \
        mv "$generated_spec_dir/SpecTE.cfg" "$artifact_dir/counterexample-SpecTE.cfg"
    python3 "$formal_dir/normalize-tlc-trace.py" \
        --model "$model" \
        --log "$log" \
        --output "$artifact_dir/counterexample.json"
fi

# TLC 1.7.4 prints `distinct-successors:evaluations`. An action such as
# `0:16` was exercised but converged on already-known states; only `*:0` is
# truly unexercised. Treating the first field as coverage creates false gates.
coverage_zero="$(grep -E '^<[^>]+>:[[:space:]]+[0-9]+:0([[:space:]]|$)' "$log" || true)"
covered_operators="$(grep -Ec '^<[^>]+>:[[:space:]]+[0-9]+:[1-9][0-9]*([[:space:]]|$)' "$log" || true)"
generated="$(sed -n 's/^\([0-9][0-9]*\) states generated,.*$/\1/p' "$log" | tail -n 1)"
distinct="$(sed -n 's/^[0-9][0-9]* states generated, \([0-9][0-9]*\) distinct states found.*$/\1/p' "$log" | tail -n 1)"
depth="$(sed -n 's/^The depth of the complete state graph search is \([0-9][0-9]*\).*$/\1/p' "$log" | tail -n 1)"
generated="${generated:-0}"
distinct="${distinct:-0}"
depth="${depth:-0}"

status=passed
if [[ "$result" -eq 124 ]]; then
    status=timeout
elif [[ "$result" -ne 0 ]]; then
    status=failed
elif [[ -n "$coverage_zero" ]]; then
    status=coverage-failed
    result=1
fi

jq -n \
    --arg schema rustos-formal-evidence-v1 \
    --arg model "$model" \
    --arg profile "$profile" \
    --arg status "$status" \
    --arg tool_version "$version" \
    --arg tool_sha256 "$sha256" \
    --arg spec_sha256 "$(sha256sum "$spec" | awk '{print $1}')" \
    --arg config_sha256 "$(sha256sum "$config" | awk '{print $1}')" \
    --arg deadlock_policy "$deadlock_policy" \
    --arg workers "$workers" \
    --argjson fingerprint "$fingerprint" \
    --argjson seed "$seed" \
    --argjson generated "$generated" \
    --argjson distinct "$distinct" \
    --argjson depth "$depth" \
    --argjson covered_operators "$covered_operators" \
    --argjson exit_code "$result" \
    '{schema:$schema,model:$model,profile:$profile,status:$status,tool:{name:"TLC",version:$tool_version,sha256:$tool_sha256},inputs:{spec_sha256:$spec_sha256,config_sha256:$config_sha256},policy:{deadlock:$deadlock_policy,workers:$workers,fingerprint:$fingerprint,seed:$seed},metrics:{generated:$generated,distinct:$distinct,depth:$depth,covered_operators:$covered_operators},exit_code:$exit_code}' > "$summary"

if [[ "$result" -ne 0 ]]; then
    echo "TLC $status model=$model; tail follows" >&2
    tail -n 80 "$log" >&2
    exit "$result"
fi
printf 'TLC passed model=%s generated=%s distinct=%s depth=%s\n' "$model" "$generated" "$distinct" "$depth"
