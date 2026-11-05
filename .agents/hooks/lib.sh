#!/usr/bin/env bash
# Shared helpers for RustOS agent hooks. Keep success paths silent and
# validation failures bounded.

rustos_repo_root() {
  cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P
}

# Run a command with a hard wall-clock bound. stdout/stderr stay outside model
# context in the caller-provided log. timeout returns 124; kill-after may return
# 137. Callers should distinguish those from an ordinary command failure.
rustos_run_bounded() {
  local seconds="$1"
  local log="$2"
  shift 2
  timeout --signal=TERM --kill-after=3s "${seconds}s" "$@" >"$log" 2>&1
}

# Produce a small diagnostic payload for hook-visible failures. Keep the first
# useful diagnostic and a tiny tail; the full log remains available to the
# caller for a focused follow-up if needed.
rustos_compact_failure() {
  local label="$1"
  local rc="$2"
  local log="$3"
  local first tail status

  if [[ "$rc" == "124" || "$rc" == "137" ]]; then
    status="timed out"
  else
    status="failed (exit ${rc})"
  fi

  first="$(grep -Em1 '(^|[^[:alpha:]])(error(\[[^]]+\])?:|fatal:|panic|FAILED|failed:|not ok -)' "$log" 2>/dev/null | head -c 768 || true)"
  tail="$(tail -n 8 "$log" 2>/dev/null | head -c 2048 || true)"

  if [[ -n "$first" && "$tail" == *"$first"* ]]; then
    first=""
  fi

  if [[ -n "$first" ]]; then
    printf '%s %s\n%s\n%s\n' "$label" "$status" "$first" "$tail"
  elif [[ -n "$tail" ]]; then
    printf '%s %s\n%s\n' "$label" "$status" "$tail"
  else
    printf '%s %s\n' "$label" "$status"
  fi
}
