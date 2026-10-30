#!/usr/bin/env bash
# Fast static preflight for formal and source-contract edits.
#
# Runs, in parallel, the registry and source-contract checks that `formal/verify-all.sh`
# otherwise reaches one at a time, so a registration mistake (unsorted id, duplicate
# REQ/HAZ, abbreviated doc path, missing flow row, stale mutation anchor) is reported in
# seconds instead of after a multi-minute sealed run. It never seals, never builds the
# product, and never writes outside the temporary log directory.
#
#   tools/agent/formal-preflight.sh           run every check, summarize failures
#   tools/agent/formal-preflight.sh --list    print the checks and exit
#
# A pass here is necessary, not sufficient: only `formal/verify-all.sh` seals.
set -uo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

# name|command (run from the repository root)
checks=(
  "diff-check|git diff --check"
  "ai-context|bash tools/agent/check-ai-context-contract.sh"
  "doc-layout|python3 tools/agent/check-doc-layout.py"
  "formal-contracts|cargo xtask formal-contracts check"
  "system-flows|bash formal/check-system-flows.sh"
  "concurrency-triangle|python3 formal/check-concurrency-triangle.py"
  "proof-index|python3 formal/check-proof-index.py"
  "proof-boundaries|python3 formal/check-proof-boundaries.py"
  "native-syscall-numbers|python3 formal/check-native-syscall-numbers.py"
  "rust-source-contracts|python3 formal/check-rust-source-contracts.py"
  "smp-source-assumptions|python3 formal/check-smp-source-assumptions.py"
  "dispatch-source-contracts|python3 formal/check-dispatch-source-contracts.py"
  "binding-exemptions|python3 formal/check-binding-exemptions.py"
  "dead-tests|python3 formal/check-dead-tests.py"
  "kernel-policy-boundary|bash formal/check-kernel-policy-boundary.sh"
  "zero-trust-ingress|bash formal/check-zero-trust-ingress.sh"
  "zero-trust-subsystems|bash formal/check-zero-trust-subsystems.sh"
  "performance-contracts|bash formal/check-performance-contracts.sh"
  "spec-mutation-registry|python3 formal/run-spec-mutations.py --check"
  "impl-mutation-registry|python3 formal/run-implementation-mutations.py --check"
)

if [[ "${1:-}" == "--list" ]]; then
  printf '%s\n' "${checks[@]}" | awk -F'|' '{printf "%-28s %s\n", $1, $2}'
  exit 0
fi
[[ "$#" -eq 0 ]] || { echo "usage: $0 [--list]" >&2; exit 2; }

log_dir="$(mktemp -d "${TMPDIR:-/tmp}/formal-preflight.XXXXXX")"
trap 'rm -rf "$log_dir"' EXIT
# Cargo takes a build lock, so launch the one cargo-backed check first and let the
# Python checks overlap with it.
start="$SECONDS"
pids=()
names=()
for entry in "${checks[@]}"; do
  name="${entry%%|*}"
  cmd="${entry#*|}"
  (
    began="$SECONDS"
    timeout 600 bash -c "$cmd" >"$log_dir/$name.log" 2>&1
    status=$?
    echo "$status $((SECONDS - began))" >"$log_dir/$name.status"
  ) &
  pids+=("$!")
  names+=("$name")
done
wait "${pids[@]}" 2>/dev/null

failed=0
for name in "${names[@]}"; do
  read -r status seconds <"$log_dir/$name.status"
  if [[ "$status" == "0" ]]; then
    printf 'ok    %-28s %3ss\n' "$name" "$seconds"
  else
    failed=$((failed + 1))
    printf 'FAIL  %-28s %3ss (exit %s)\n' "$name" "$seconds" "$status"
    # The first useful diagnostic and a tiny tail, not the whole log.
    grep -v -e '^$' -e 'large-file debt' "$log_dir/$name.log" | head -4 | sed 's/^/        /'
  fi
done
printf 'formal preflight: %d of %d checks failed in %ss\n' \
  "$failed" "${#checks[@]}" "$((SECONDS - start))"
exit $(( failed == 0 ? 0 : 1 ))
