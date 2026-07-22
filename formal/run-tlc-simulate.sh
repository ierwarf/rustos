#!/usr/bin/env bash
# Nightly-only long-trace simulation for registry-selected models.
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: bash formal/run-tlc-simulate.sh <model/path-without-extension>" >&2
    exit 2
fi
model="$1"
repo_root="$(git rev-parse --show-toplevel)"
formal_dir="$repo_root/formal"
row="$(awk -F '\t' -v wanted="$model" '$1 == wanted { print; found++ } END { if (found != 1) exit 1 }' "$formal_dir/models.tsv")" || {
    echo "model is not uniquely registered: $model" >&2
    exit 2
}
IFS=$'\t' read -r _model _class deadlock_policy _reason _pr_timeout nightly_timeout nightly_mode _apalache _tlaps _trace <<< "$row"
[[ "$nightly_mode" == exhaustive+simulate ]] || { echo "model is not admitted for nightly simulation: $model" >&2; exit 2; }

lock="$formal_dir/tla2tools.lock"
version="$(sed -n 's/^version=//p' "$lock" | head -n 1)"
sha256="$(sed -n 's/^sha256=//p' "$lock" | head -n 1)"
jar="${TLA_CACHE_DIR:-$HOME/.cache/rustos/tla}/tla2tools-$version.jar"
[[ -f "$jar" ]] && printf '%s  %s\n' "$sha256" "$jar" | sha256sum --check --status || {
    echo "missing or invalid pinned TLC jar; run one exhaustive model first" >&2
    exit 2
}

spec="$formal_dir/$model.tla"
config="$formal_dir/$model.cfg"
artifact_dir="${TLA_ARTIFACT_DIR:-$repo_root/build/formal/tlc/nightly-sim/${model//\//__}}"
mkdir -p "$artifact_dir"
log="$artifact_dir/tlc-simulate.log"
summary="$artifact_dir/summary.json"
state_dir="$(mktemp -d "${TMPDIR:-/tmp}/rustos-tlc-sim.XXXXXX")"
trap 'rm -rf "$state_dir"' EXIT
workers="${TLC_WORKERS:-1}"
seed="${TLC_SIM_SEED:-20260722}"
traces="${TLC_SIM_TRACES:-2000}"
depth="${TLC_SIM_DEPTH:-200}"
for value in "$seed" "$traces" "$depth"; do
    [[ "$value" =~ ^[1-9][0-9]*$ ]] || { echo "simulation values must be positive integers" >&2; exit 2; }
done

deadlock_args=()
[[ "$deadlock_policy" == intentional-terminal ]] && deadlock_args=(-deadlock)
set +e
(
    cd "$artifact_dir"
    timeout --signal=TERM --kill-after=10 "$nightly_timeout" \
        java -XX:+UseParallelGC -jar "$jar" \
        -workers "$workers" \
        -seed "$seed" \
        -depth "$depth" \
        -coverage 1 \
        "${deadlock_args[@]}" \
        -metadir "$state_dir" \
        -config "$config" \
        -simulate "file=$artifact_dir/traces,num=$traces" \
        "$spec"
) >"$log" 2>&1
result=$?
set -e
status=passed
[[ "$result" -eq 124 ]] && status=timeout
[[ "$result" -ne 0 && "$result" -ne 124 ]] && status=failed
jq -n \
    --arg schema rustos-formal-simulation-v1 \
    --arg model "$model" \
    --arg status "$status" \
    --arg tool_version "$version" \
    --argjson seed "$seed" \
    --argjson traces "$traces" \
    --argjson depth "$depth" \
    --argjson exit_code "$result" \
    '{schema:$schema,model:$model,status:$status,tool:{name:"TLC",version:$tool_version},simulation:{seed:$seed,traces:$traces,depth:$depth},exit_code:$exit_code,claim:"randomized bug-finding only; not exhaustive evidence"}' > "$summary"
if [[ "$result" -ne 0 ]]; then
    echo "TLC simulation $status model=$model; tail follows" >&2
    tail -n 80 "$log" >&2
    exit "$result"
fi
printf 'TLC simulation passed model=%s traces=%s depth=%s\n' "$model" "$traces" "$depth"
