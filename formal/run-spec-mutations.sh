#!/usr/bin/env bash
# Mutation adequacy gate for named TLA+ safety properties and transitions.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
exec python3 "$repo_root/formal/run-spec-mutations.py" "$@"
