#!/usr/bin/env bash
# Instrument host-testable kernel and service policy with the pinned Rust
# toolchain.  This is a bounded test profile, never a production build shape.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
toolchain="$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)"
registry=formal/sanitizer-targets.tsv
profile="${1:---profile=all}"
case "$profile" in
    --profile=address) selected=address ;;
    --profile=thread) selected=thread ;;
    --profile=all) selected=all ;;
    *)
        echo "usage: $0 [--profile=address|thread|all]" >&2
        exit 2
        ;;
esac

command -v rustup >/dev/null 2>&1 || {
    echo "missing rustup" >&2
    exit 2
}
rustc "+$toolchain" -Z help 2>/dev/null | grep -q 'sanitizer' || {
    echo "pinned toolchain does not expose -Zsanitizer" >&2
    exit 2
}
[[ -s "$registry" ]] || {
    echo "missing sanitizer target registry" >&2
    exit 1
}

artifact_dir="${SANITIZER_ARTIFACT_DIR:-$repo_root/build/formal/sanitizers}"
mkdir -p "$artifact_dir"
if [[ "$selected" == all ]]; then
    summary_path="$artifact_dir/summary.json"
else
    summary_path="$artifact_dir/summary-$selected.json"
fi
executed=0
declare -a result_rows=()
declare -A seen_ids=()
declare -A seen_targets=()
declare -A executed_profiles=()
last_id=

while IFS=$'\t' read -r id sanitizer package features test_target severity owner extra; do
    [[ -n "$id" && "$id" != \#* ]] || continue
    [[ -z "${extra:-}" ]] || {
        echo "$id: invalid sanitizer registry column count" >&2
        exit 1
    }
    [[ "$sanitizer" == address || "$sanitizer" == thread ]] || {
        echo "$id: unsupported sanitizer $sanitizer" >&2
        exit 1
    }
    [[ -z "${seen_ids[$id]:-}" ]] || {
        echo "$id: duplicate sanitizer target id" >&2
        exit 1
    }
    [[ -z "$last_id" || "$last_id" < "$id" ]] || {
        echo "$id: sanitizer target ids must be sorted and unique" >&2
        exit 1
    }
    seen_ids["$id"]=1
    last_id="$id"
    target_identity="$sanitizer:$package:$features:$test_target"
    [[ -z "${seen_targets[$target_identity]:-}" ]] || {
        echo "$id: duplicate sanitizer target $target_identity" >&2
        exit 1
    }
    seen_targets["$target_identity"]=1
    [[ "$severity" == critical || "$severity" == high ]] || {
        echo "$id: target is not critical/high" >&2
        exit 1
    }
    [[ "$test_target" == lib || "$test_target" == all ]] || {
        echo "$id: unsupported test target $test_target" >&2
        exit 1
    }
    if [[ "$selected" != all && "$selected" != "$sanitizer" ]]; then
        continue
    fi

    log="$artifact_dir/$id.log"
    target_dir="$artifact_dir/target-$sanitizer-buildstd-v1"
    declare -a command=(
        cargo "+$toolchain" test -q
        -Zbuild-std
        --target x86_64-unknown-linux-gnu
        -p "$package"
    )
    if [[ "$test_target" == lib ]]; then
        command+=(--lib)
    fi
    if [[ "$features" != - ]]; then
        command+=(--features "$features")
    fi
    if ! timeout --foreground 180s env \
        CARGO_TARGET_DIR="$target_dir" \
        CARGO_PROFILE_TEST_PANIC=unwind \
        RUSTFLAGS="-Zsanitizer=$sanitizer -Cforce-frame-pointers=yes" \
        RUSTDOCFLAGS="-Zsanitizer=$sanitizer -Cforce-frame-pointers=yes" \
        RUST_TEST_THREADS=1 \
        "${command[@]}" >"$log" 2>&1; then
        tail -n 100 "$log" >&2
        exit 1
    fi
    result_rows+=("$id"$'\t'"$sanitizer"$'\t'"$package"$'\t'"$features"$'\t'"$test_target"$'\t'"$severity"$'\t'"$owner")
    executed_profiles["$sanitizer"]=1
    executed=$((executed + 1))
done <"$registry"

[[ "$executed" -gt 0 ]] || {
    echo "sanitizer profile selected no targets" >&2
    exit 1
}
if [[ "$selected" == all ]] &&
    [[ -z "${executed_profiles[address]:-}" || -z "${executed_profiles[thread]:-}" ]]; then
    echo "sanitizer all profile must execute address and thread targets" >&2
    exit 1
fi

results_file="$artifact_dir/results-$selected.tsv"
printf '%s\n' "${result_rows[@]}" >"$results_file"
python3 - "$summary_path" "$toolchain" "$selected" "$registry" "$results_file" <<'PY'
from __future__ import annotations

import hashlib
import json
import sys
from pathlib import Path

output = Path(sys.argv[1])
toolchain = sys.argv[2]
profile = sys.argv[3]
registry = Path(sys.argv[4])
results = Path(sys.argv[5])
targets = []
for line in results.read_text().splitlines():
    line = line.rstrip("\n")
    if not line:
        continue
    ident, sanitizer, package, features, target, severity, owner = line.split("\t")
    targets.append(
        {
            "id": ident,
            "sanitizer": sanitizer,
            "package": package,
            "features": features,
            "target": target,
            "severity": severity,
            "owner": owner,
        }
    )
summary = {
    "schema": "rustos-sanitizer-evidence-v1",
    "status": "passed",
    "toolchain": toolchain,
    "profile": profile,
    "registry_sha256": hashlib.sha256(registry.read_bytes()).hexdigest(),
    "targets": targets,
}
output.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
PY

printf 'sanitizer profile passed profile=%s targets=%s\n' "$selected" "$executed"
