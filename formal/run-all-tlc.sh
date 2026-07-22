#!/usr/bin/env bash
# Run every registry-admitted RustOS formal model.
set -euo pipefail

profile=pr
if [[ $# -eq 2 && "$1" == --profile ]]; then
    profile="$2"
elif [[ $# -ne 0 ]]; then
    echo "usage: bash formal/run-all-tlc.sh [--profile pr|nightly]" >&2
    exit 2
fi
[[ "$profile" == pr || "$profile" == nightly ]] || { echo "invalid TLC profile: $profile" >&2; exit 2; }

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
bash formal/selftest.sh

while IFS=$'\t' read -r model _class _deadlock _reason _pr_timeout _nightly_timeout nightly_mode _apalache _tlaps _trace; do
    [[ -z "$model" || "$model" == \#* ]] && continue
    bash formal/run-tlc.sh --profile "$profile" "$model"
    if [[ "$profile" == nightly && "$nightly_mode" == exhaustive+simulate ]]; then
        bash formal/run-tlc-simulate.sh "$model"
    fi
done < formal/models.tsv
