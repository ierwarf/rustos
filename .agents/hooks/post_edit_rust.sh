#!/usr/bin/env bash
# Shared PostToolUse hook for Rust edits, wired from Codex and Claude.
#
# Cheap contract lint runs for each new Rust workspace state. The heavier
# `cargo xtask check` is content-cached and coalesced during edit bursts; the
# shell pre-commit gate rechecks any state that was skipped here.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
# shellcheck source=.agents/hooks/lib.sh
source "$SCRIPT_DIR/lib.sh"

INPUT="$(cat)"
path=""
cmd=""
field_index=0
while IFS= read -r -d '' field; do
  if (( field_index == 0 )); then
    path="$field"
  else
    cmd="$field"
    break
  fi
  field_index=$((field_index + 1))
done < <(printf '%s' "$INPUT" | jq -jr '
  (.tool_input.file_path // .tool_input.relative_path //
   .arguments.file_path // .arguments.relative_path //
   .params.file_path // .params.relative_path // ""), "\u0000",
  (.tool_input.command // .arguments.command // .params.command // ""), "\u0000"
' 2>/dev/null || true)

REPO_ROOT="$(rustos_repo_root)"
case "$path" in
  /*) abs_path="$path" ;;
  "") abs_path="" ;;
  *) abs_path="$REPO_ROOT/$path" ;;
esac

case "$abs_path" in
  "$REPO_ROOT"/*.rs) ;;
  *)
    if ! printf '%s\n' "$cmd" | grep -Eq '^\*\*\* (Add|Update|Delete) File: .+\.rs$'; then
      exit 0
    fi
    ;;
esac

cd "$REPO_ROOT"
fingerprint="$(rustos_workspace_fingerprint "$REPO_ROOT")"
stamp="$(rustos_read_check_stamp "$REPO_ROOT")"
if [[ -n "$stamp" && "$stamp" == "$fingerprint" ]]; then
  exit 0
fi

log="$(mktemp)"
trap 'rm -f "$log"' EXIT

emit_failure() {
  local label="$1"
  local rc="$2"
  local message
  message="$(rustos_compact_failure "$label" "$rc" "$log")"
  jq -n --arg m "$message" '{systemMessage:$m}'
  exit 0
}

# This is intentionally first: it is cheap and catches structural contract
# drift even when the heavier type/workspace check is coalesced.
if rustos_run_bounded 8 "$log" python3 formal/check-rust-source-contracts.py; then
  :
else
  emit_failure "source-contract lint" "$?"
fi

# Avoid making a burst of small edits pay the full workspace check each time.
# This is only a latency optimization: pre-commit compares the same content
# fingerprint and runs the check if this hook did not validate the final state.
state_dir="$(rustos_hook_state_dir "$REPO_ROOT")"
last_path="$(rustos_last_check_path "$REPO_ROOT")"
interval="${RUSTOS_POST_EDIT_CHECK_INTERVAL_SEC:-20}"
now="$(date +%s)"
last="$(cat "$last_path" 2>/dev/null || echo 0)"
if [[ "$interval" =~ ^[0-9]+$ ]] && (( interval > 0 )) \
  && [[ "$last" =~ ^[0-9]+$ ]] && (( now - last < interval )); then
  printf '%s\n' "$fingerprint" >"$state_dir/xtask-check.pending"
  exit 0
fi
printf '%s\n' "$now" >"$last_path"

if rustos_run_bounded 50 "$log" cargo xtask check; then
  rustos_write_check_stamp "$REPO_ROOT" "$fingerprint"
  rm -f "$state_dir/xtask-check.pending"
  exit 0
else
  emit_failure "cargo xtask check" "$?"
fi
