#!/usr/bin/env bash
# Check x86_64 weak-memory litmus baselines and their required killed mutants.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
artifact_dir="${HERD_ARTIFACT_DIR:-$repo_root/build/formal/herd}"
mkdir -p "$artifact_dir"

python3 formal/check-concurrency-triangle.py

read -r pinned_version default_seconds < <(
    python3 - <<'PY'
import tomllib
with open("formal/herdtools.lock", "rb") as handle:
    lock = tomllib.load(handle)
with open("formal/concurrency-triangle.toml", "rb") as handle:
    budget = tomllib.load(handle)["budget"]
print(lock["version"], budget["herd_max_seconds"])
PY
)
seconds="${HERD_MAX_SECONDS:-$default_seconds}"
[[ "$seconds" =~ ^[0-9]+$ && "$seconds" -ge 1 && "$seconds" -le 60 ]] || {
    echo "HERD_MAX_SECONDS must be in 1..60" >&2
    exit 2
}

canonical_herd="$repo_root/build/formal/tools/herdtools7-$pinned_version/usr/bin/herd7"
herd_bin="${HERD7:-}"
if [[ -z "$herd_bin" && -x "$canonical_herd" ]]; then
    herd_bin="$canonical_herd"
fi
if [[ -z "$herd_bin" ]]; then
    herd_bin="$(command -v herd7 || true)"
fi
[[ -n "$herd_bin" && -x "$herd_bin" ]] || {
    echo "pinned herd7 $pinned_version is required; run bash formal/setup-herdtools.sh or set HERD7" >&2
    exit 1
}
version_output="$($herd_bin -version 2>&1)"
[[ "$version_output" == "$pinned_version,"* || "$version_output" == "$pinned_version "* ]] || {
    echo "unexpected herd7 version: $version_output (expected $pinned_version)" >&2
    exit 1
}
reported_libdir="$($herd_bin -libdir)"
libdir="$reported_libdir"
if [[ ! -d "$libdir" ]]; then
    libdir="$repo_root/build/formal/tools/herdtools7-$pinned_version-source/herd/libdir"
fi
[[ -d "$libdir" ]] || {
    echo "herd7 cat model directory is unavailable ($reported_libdir); run bash formal/setup-herdtools.sh" >&2
    exit 1
}

mapfile -t cases < <(
    python3 - <<'PY'
import tomllib
with open("formal/concurrency-triangle.toml", "rb") as handle:
    for scenario in tomllib.load(handle)["scenario"]:
        if "herd_test" in scenario:
            print(f"{scenario['id']}\t{scenario['herd_test']}\t{scenario['herd_mutant']}\t{scenario['herd_cat']}\t{scenario['source']}")
PY
)

result_dir="$artifact_dir/results"
rm -rf "$result_dir"
mkdir -p "$result_dir"
for case in "${cases[@]}"; do
    IFS=$'\t' read -r ident baseline mutant cat_name source <<<"$case"
    cat_path="$libdir/$cat_name"
    [[ -f "$cat_path" ]] || { echo "$ident: missing pinned herd cat model $cat_path" >&2; exit 1; }
    baseline_log="$result_dir/$ident.baseline.log"
    mutant_log="$result_dir/$ident.mutant.log"
    if ! timeout --preserve-status "$seconds" "$herd_bin" -set-libdir "$libdir" -cat "$cat_path" "$baseline" >"$baseline_log" 2>&1; then
        tail -n 80 "$baseline_log" >&2
        exit 1
    fi
    if ! timeout --preserve-status "$seconds" "$herd_bin" -set-libdir "$libdir" -cat "$cat_path" "$mutant" >"$mutant_log" 2>&1; then
        tail -n 80 "$mutant_log" >&2
        exit 1
    fi
    rg -q '^Positive: 0 Negative: [1-9][0-9]*$' "$baseline_log" || {
        echo "$ident: baseline did not forbid its explicit bad outcome" >&2
        exit 1
    }
    rg -q '^Observation .+ Never 0 [1-9][0-9]*$' "$baseline_log" || {
        echo "$ident: baseline observation is not a strict Never" >&2
        exit 1
    }
    rg -q '^Positive: [1-9][0-9]* Negative: [0-9][0-9]*$' "$mutant_log" || {
        echo "$ident: ordering mutant survived; litmus assertion is vacuous" >&2
        exit 1
    }
    rg -q '^Observation .+ Sometimes [1-9][0-9]* [0-9][0-9]*$' "$mutant_log" || {
        echo "$ident: ordering mutant did not produce a reachable counterexample" >&2
        exit 1
    }
    jq -n \
        --arg id "$ident" \
        --arg source "$source" \
        --arg baseline "$baseline" \
        --arg mutant "$mutant" \
        --arg cat "$cat_name" \
        --arg baseline_sha256 "$(sha256sum "$baseline_log" | awk '{print $1}')" \
        --arg mutant_sha256 "$(sha256sum "$mutant_log" | awk '{print $1}')" \
        '{id:$id,source:$source,baseline:$baseline,mutant:$mutant,cat:$cat,baseline_log_sha256:$baseline_sha256,mutant_log_sha256:$mutant_sha256}' \
        >"$result_dir/$ident.json"
done

jq -s \
    --arg version "$pinned_version" \
    --arg herd "$herd_bin" \
    --argjson per_test_max_seconds "$seconds" \
    --arg registry_sha256 "$(sha256sum formal/concurrency-triangle.toml | awk '{print $1}')" \
    --arg lock_sha256 "$(sha256sum formal/herdtools.lock | awk '{print $1}')" \
    '{schema:"rustos-herd7-evidence-v1",status:"passed",architecture:"x86_64",herd_version:$version,herd_binary:$herd,per_test_max_seconds:$per_test_max_seconds,registry_sha256:$registry_sha256,lock_sha256:$lock_sha256,tests:.}' \
    "$result_dir"/*.json >"$artifact_dir/summary.json"
printf 'herd7 x86_64 litmus and ordering mutants passed\n'
