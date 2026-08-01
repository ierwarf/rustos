#!/usr/bin/env bash
# Run the profile-selected RustOS TLC model set within its declared wall budget.
set -euo pipefail

profile=pr
if [[ $# -eq 2 && "$1" == --profile ]]; then
    profile="$2"
elif [[ $# -ne 0 ]]; then
    echo "usage: bash formal/run-all-tlc.sh [--profile pr|smp-iteration|nightly]" >&2
    exit 2
fi
[[ "$profile" == pr || "$profile" == smp-iteration || "$profile" == nightly ]] || {
    echo "invalid TLC profile: $profile" >&2
    exit 2
}

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
started="$SECONDS"
if [[ "${FORMAL_SELFTEST_ALREADY_PASSED:-0}" != 1 ]]; then
    bash formal/selftest.sh
fi

mapfile -t models < <(
    python3 - "$repo_root/formal/contracts.toml" "$profile" "$repo_root/formal/models.tsv" <<'PY'
import sys
import tomllib
from pathlib import Path

contracts = tomllib.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
profile = contracts["profiles"][sys.argv[2]]
if sys.argv[2] in {"pr", "smp-iteration"}:
    models = profile["required_models"]
else:
    models = [
        line.split("\t", 1)[0]
        for line in Path(sys.argv[3]).read_text(encoding="utf-8").splitlines()
        if line and not line.startswith("#")
    ]
for model in models:
    print(model)
PY
)
[[ "${#models[@]}" -gt 0 ]] || { echo "TLC profile $profile selects no models" >&2; exit 2; }
budget_seconds="$(python3 - "$repo_root/formal/contracts.toml" "$profile" <<'PY'
import sys
import tomllib
from pathlib import Path
print(tomllib.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))["profiles"][sys.argv[2]]["tlc_max_wall_seconds"])
PY
)"
[[ "$budget_seconds" =~ ^[1-9][0-9]*$ ]] || { echo "invalid TLC wall budget for $profile" >&2; exit 2; }
for model in "${models[@]}"; do
    elapsed=$((SECONDS - started))
    remaining=$((budget_seconds - elapsed))
    (( remaining > 0 )) || {
        echo "TLC profile $profile exceeded its ${budget_seconds}s wall budget before $model" >&2
        exit 124
    }
    if python3 formal/tlc_cache.py \
        --root "$repo_root" --profile "$profile" --model "$model"; then
        continue
    fi
    timeout --preserve-status --signal=TERM --kill-after=5 "$remaining" \
        bash formal/run-tlc.sh --profile "$profile" "$model"
    if [[ "$profile" == nightly ]]; then
        nightly_mode="$(awk -F '\t' -v wanted="$model" '$1 == wanted { print $7 }' formal/models.tsv)"
        if [[ "$nightly_mode" == exhaustive+simulate ]]; then
            elapsed=$((SECONDS - started))
            remaining=$((budget_seconds - elapsed))
            (( remaining > 0 )) || {
                echo "TLC profile $profile exceeded its ${budget_seconds}s wall budget before simulation for $model" >&2
                exit 124
            }
            timeout --preserve-status --signal=TERM --kill-after=5 "$remaining" \
                bash formal/run-tlc-simulate.sh "$model"
        fi
    fi
done
(( SECONDS - started <= budget_seconds )) || {
    echo "TLC profile $profile exceeded its ${budget_seconds}s wall budget" >&2
    exit 124
}
printf 'TLC profile passed profile=%s models=%s wall_budget_seconds=%s elapsed_seconds=%s\n' \
    "$profile" "${#models[@]}" "$budget_seconds" "$((SECONDS - started))"
