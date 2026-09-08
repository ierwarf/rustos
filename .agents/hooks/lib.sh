#!/usr/bin/env bash
# Shared helpers for RustOS agent hooks. Keep success paths silent and make
# cache keys content-based so hooks can skip duplicate validation safely.

rustos_repo_root() {
  cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P
}

rustos_hook_state_dir() {
  local root="$1"
  local key
  key="$(printf '%s' "$root" | sha256sum | awk '{print $1}')"
  local dir="${TMPDIR:-/tmp}/rustos-agent-hooks-${key}"
  mkdir -p "$dir"
  chmod 700 "$dir" 2>/dev/null || true
  printf '%s\n' "$dir"
}

# Fingerprint the exact repository state without serializing whole diffs into a
# temporary string. Index object ids cover staged content; dirty/untracked
# worktree files are hashed directly. Generated/ignored trees are excluded by
# git, matching the repository's agent policy.
rustos_workspace_fingerprint() {
  local root="$1"
  (
    cd "$root"
    {
      printf 'head:'
      git rev-parse --verify HEAD
      printf 'index:\n'
      git diff --cached --raw --no-abbrev --no-renames
      printf 'worktree-meta:\n'
      git diff --raw --no-abbrev --no-renames
      {
        git diff --name-only -z --no-renames
        git ls-files --others --exclude-standard -z
      } | sort -zu | while IFS= read -r -d '' file; do
        printf 'path:%s\0' "$file"
        if [[ -e "$file" || -L "$file" ]]; then
          git hash-object --no-filters -- "$file" 2>/dev/null || printf 'unhashable\n'
        else
          printf 'deleted\n'
        fi
      done
    } | sha256sum | awk '{print $1}'
  )
}

rustos_check_stamp_path() {
  local root="$1"
  printf '%s/xtask-check.ok\n' "$(rustos_hook_state_dir "$root")"
}

rustos_last_check_path() {
  local root="$1"
  printf '%s/xtask-check.last\n' "$(rustos_hook_state_dir "$root")"
}

rustos_read_check_stamp() {
  local root="$1"
  cat "$(rustos_check_stamp_path "$root")" 2>/dev/null || true
}

rustos_write_check_stamp() {
  local root="$1"
  local fingerprint="$2"
  printf '%s\n' "$fingerprint" >"$(rustos_check_stamp_path "$root")"
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

  first="$(grep -Enm1 '(^|[^[:alpha:]])(error(\[[^]]+\])?:|fatal:|panic|FAILED|failed:|not ok -)' "$log" 2>/dev/null || true)"
  tail="$(tail -n 12 "$log" 2>/dev/null | head -c 3072 || true)"

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
