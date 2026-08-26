#!/usr/bin/env bash
# Run bounded Rust proofs, require witness coverage, and retain normalized evidence.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
export PATH="$cargo_home/bin:$PATH"
lock="$repo_root/formal/kani.lock"
version="$(sed -n 's/^version=//p' "$lock" | head -n 1)"
[[ -n "$version" ]] || { echo "invalid $lock" >&2; exit 2; }
command -v cargo-kani >/dev/null 2>&1 || { echo "missing cargo-kani; run: bash formal/setup-kani.sh" >&2; exit 2; }
installed="$(cargo kani --version | awk 'NR == 1 { print $2 }')"
[[ "$installed" == "$version" ]] || { echo "cargo-kani version $installed does not match pinned $version" >&2; exit 2; }

cd "$repo_root"
artifact_dir="${KANI_ARTIFACT_DIR:-$repo_root/build/formal/kani}"
mkdir -p "$artifact_dir"
cache_dir="${KANI_CACHE_DIR:-$repo_root/build/formal/kani-cache-v1}"
mkdir -p "$cache_dir"
if [[ "${FORMAL_PROOF_INDEX_ALREADY_PASSED:-0}" != 1 ]]; then
    bash formal/run-proof-index.sh
fi

packages=(runtime-control rustos-image-admission driver-domain-protocol rustos-user-abi)
cache_key_args=()
for package in "${packages[@]}"; do
    cache_key_args+=(--package "$package")
done
cache_keys="$(python3 formal/kani-cache-key.py \
    --root "$repo_root" \
    --version "$version" \
    "${cache_key_args[@]}")"
overall=0
cache_hits=0
cache_misses=0
for package in "${packages[@]}"; do
    log="$artifact_dir/$package.log"
    cache_key="$(jq -er --arg package "$package" '.[$package]' <<<"$cache_keys")"
    cached_log="$cache_dir/$package-$cache_key.log"
    cached_manifest="$cache_dir/$package-$cache_key.json"
    rm -f "$artifact_dir/$package-playback.log"
    if [[ -s "$cached_log" && -s "$cached_manifest" ]] \
        && [[ "$(jq -er '.schema' "$cached_manifest")" == rustos-kani-package-cache-v1 ]] \
        && [[ "$(jq -er '.status' "$cached_manifest")" == passed ]] \
        && [[ "$(jq -er '.package' "$cached_manifest")" == "$package" ]] \
        && [[ "$(jq -er '.input_sha256' "$cached_manifest")" == "$cache_key" ]] \
        && [[ "$(jq -er '.log_sha256' "$cached_manifest")" == "$(sha256sum "$cached_log" | awk '{print $1}')" ]] \
        && ! rg -q '\*\* 0 of [1-9][0-9]* cover properties satisfied|VERIFICATION:- (FAILED|UNDETERMINED)' "$cached_log"; then
        cp "$cached_log" "$log"
        cache_hits=$((cache_hits + 1))
        continue
    fi
    cache_misses=$((cache_misses + 1))
    set +e
    cargo kani -p "$package" \
        --output-format terse \
        --harness-timeout 180 \
        -Z unstable-options \
        --run-sanity-checks >"$log" 2>&1
    result=$?
    set -e
    if [[ "$result" -ne 0 ]]; then
        overall=1
        set +e
        cargo kani -p "$package" \
            --output-format terse \
            --concrete-playback print \
            -Z concrete-playback \
            -Z unstable-options \
            --run-sanity-checks >"$artifact_dir/$package-playback.log" 2>&1
        set -e
    elif ! rg -q '\*\* 0 of [1-9][0-9]* cover properties satisfied|VERIFICATION:- (FAILED|UNDETERMINED)' "$log"; then
        cache_tmp="$cache_dir/.$package-$cache_key.$$.tmp"
        cp "$log" "$cache_tmp"
        chmod 0644 "$cache_tmp"
        mv -f "$cache_tmp" "$cached_log"
        manifest_tmp="$cache_dir/.$package-$cache_key.$$.json.tmp"
        jq -n \
            --arg schema rustos-kani-package-cache-v1 \
            --arg status passed \
            --arg package "$package" \
            --arg input_sha256 "$cache_key" \
            --arg log_sha256 "$(sha256sum "$cached_log" | awk '{print $1}')" \
            '{schema:$schema,status:$status,package:$package,input_sha256:$input_sha256,log_sha256:$log_sha256}' \
            > "$manifest_tmp"
        chmod 0644 "$manifest_tmp"
        mv -f "$manifest_tmp" "$cached_manifest"
    fi
done

if rg -n '\*\* 0 of [1-9][0-9]* cover properties satisfied|VERIFICATION:- (FAILED|UNDETERMINED)' "$artifact_dir"/*.log >"$artifact_dir/unmet-witnesses.txt"; then
    overall=1
else
    : >"$artifact_dir/unmet-witnesses.txt"
fi

python3 formal/normalize-kani-results.py \
    --version "$version" \
    --logs "$artifact_dir" \
    --sarif "$artifact_dir/kani.sarif" \
    --summary "$artifact_dir/summary.json"
jq --arg proof_index_sha256 "$(sha256sum formal/proof-index.toml | awk '{print $1}')" \
    '. + {proof_index_sha256:$proof_index_sha256}' \
    "$artifact_dir/summary.json" >"$artifact_dir/summary.next.json"
mv "$artifact_dir/summary.next.json" "$artifact_dir/summary.json"

if [[ "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["status"])' "$artifact_dir/summary.json")" != "passed" ]]; then
    overall=1
fi

if [[ "$overall" -ne 0 ]]; then
    echo "Kani proof or witness coverage failed; inspect $artifact_dir" >&2
    for log in "$artifact_dir"/*.log; do
        tail -n 40 "$log" >&2
    done
    exit 1
fi
printf 'Kani passed packages=%s cache_hits=%s cache_misses=%s evidence=%s\n' \
    "${#packages[@]}" "$cache_hits" "$cache_misses" "$artifact_dir/summary.json"
