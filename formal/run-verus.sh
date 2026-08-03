#!/usr/bin/env bash
# Verify every indexed unbounded proof kernel with fixed solver/wall budgets.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
lock="$repo_root/formal/verus.lock"

read_lock_value() {
    local key="$1"
    sed -n "s/^$key=//p" "$lock" | head -n 1
}

version="$(read_lock_value version)"
cache_root="${VERUS_CACHE_DIR:-$HOME/.cache/rustos/verus}"
binary="$cache_root/$version/verus-x86-linux/verus"
if [[ -z "$version" || ! -x "$binary" ]]; then
    echo "missing pinned Verus; run: bash formal/setup-verus.sh" >&2
    exit 2
fi
installed="$($binary --version | sed -n 's/^  Version: //p' | head -n 1)"
if [[ "$installed" != "$version" ]]; then
    echo "Verus version $installed does not match pinned $version" >&2
    exit 2
fi

cd "$repo_root"
artifact_dir="${VERUS_ARTIFACT_DIR:-$repo_root/build/formal/verus}"
mkdir -p "$artifact_dir"
if [[ "${FORMAL_PROOF_INDEX_ALREADY_PASSED:-0}" != 1 ]]; then
    bash formal/run-proof-index.sh
fi
mapfile -t cases < <(
    python3 - <<'PY'
import tomllib
with open("formal/proof-index.toml", "rb") as handle:
    for proof in tomllib.load(handle)["proof"]:
        if proof["kind"] == "verus":
            print(f"{proof['id']}\t{proof['proof_file']}")
PY
)

result_dir="$artifact_dir/results"
rm -rf "$result_dir"
mkdir -p "$result_dir"
for case in "${cases[@]}"; do
    IFS=$'\t' read -r ident proof_file <<<"$case"
    log="$result_dir/$ident.log"
    if ! timeout --preserve-status 60 "$binary" --rlimit 150 "$proof_file" >"$log" 2>&1; then
        tail -n 80 "$log" >&2
        exit 1
    fi
    rg -q '^verification results:: [1-9][0-9]* verified, 0 errors$' "$log" || {
        echo "$ident: Verus did not report a verified nonempty proof set" >&2
        exit 1
    }
    jq -n --arg id "$ident" --arg proof_file "$proof_file" \
        --arg log_sha256 "$(sha256sum "$log" | awk '{print $1}')" \
        '{id:$id,proof_file:$proof_file,log_sha256:$log_sha256}' \
        >"$result_dir/$ident.json"
done
jq -s --arg version "$version" \
    --arg proof_index_sha256 "$(sha256sum formal/proof-index.toml | awk '{print $1}')" \
    '{schema:"rustos-verus-evidence-v2",status:"passed",tool:{name:"Verus",version:$version},solver_rlimit:150,per_file_max_seconds:60,proof_index_sha256:$proof_index_sha256,proofs:.}' \
    "$result_dir"/*.json >"$artifact_dir/summary.json"
printf 'Verus proof kernels passed files=%s version=%s\n' "$(printf '%s\n' "${cases[@]}" | wc -l)" "$version"
