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

packages=(runtime-control rustos-image-admission driver-domain-protocol)
overall=0
for package in "${packages[@]}"; do
    log="$artifact_dir/$package.log"
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
printf 'Kani passed packages=%s evidence=%s\n' "${#packages[@]}" "$artifact_dir/summary.json"
