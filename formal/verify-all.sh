#!/usr/bin/env bash
# Full PR-sized formal gate: executable TLA+ models plus Rust code proofs.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"
bash formal/run-all-tlc.sh
bash formal/run-kani.sh
bash formal/run-verus.sh
