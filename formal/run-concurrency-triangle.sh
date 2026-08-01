#!/usr/bin/env bash
# Run the complementary bounded concurrency gates before any QEMU evidence.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
artifact_dir="${CONCURRENCY_TRIANGLE_ARTIFACT_DIR:-$repo_root/build/formal/concurrency-triangle}"
mkdir -p "$artifact_dir"

python3 formal/check-concurrency-triangle.py
default_branches="$(python3 - <<'PY'
import tomllib
with open("formal/concurrency-triangle.toml", "rb") as handle:
    print(tomllib.load(handle)["budget"]["loom_max_branches"])
PY
)"
branches="${LOOM_MAX_BRANCHES:-$default_branches}"
[[ "$branches" =~ ^[0-9]+$ && "$branches" -ge 1 && "$branches" -le 10000 ]] || {
    echo "LOOM_MAX_BRANCHES must be in 1..10000" >&2
    exit 2
}

LOOM_MAX_BRANCHES="$branches" bash formal/run-loom.sh
bash formal/run-shuttle.sh
bash formal/run-herd.sh

jq -n \
    --arg loom_sha256 "$(sha256sum build/formal/loom/summary.json | awk '{print $1}')" \
    --arg shuttle_sha256 "$(sha256sum build/formal/shuttle/summary.json | awk '{print $1}')" \
    --arg herd_sha256 "$(sha256sum build/formal/herd/summary.json | awk '{print $1}')" \
    --arg registry_sha256 "$(sha256sum formal/concurrency-triangle.toml | awk '{print $1}')" \
    '{schema:"rustos-concurrency-triangle-evidence-v1",status:"passed",registry_sha256:$registry_sha256,loom_summary_sha256:$loom_sha256,shuttle_summary_sha256:$shuttle_sha256,herd_summary_sha256:$herd_sha256}' \
    >"$artifact_dir/summary.json"
printf 'Loom + Shuttle + herd7 concurrency triangle passed\n'
