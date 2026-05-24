#!/usr/bin/env bash
# Codex PostToolUse hook for Edit/Write.
#
# After Rust source edits, run a fast type check so the agent loop gets
# immediate feedback instead of discovering the break much later. Only
# runs inside the RustOS workspace; no-op elsewhere.

set -euo pipefail

INPUT="$(cat)"

path="$(printf '%s' "$INPUT" | jq -r '
  .tool_input.file_path // .arguments.file_path // .params.file_path // empty
' 2>/dev/null || true)"

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
case "$path" in
  /*) abs_path="$path" ;;
  "") abs_path="" ;;
  *) abs_path="$REPO_ROOT/$path" ;;
esac

# Only react to *.rs edits inside this workspace.
case "$abs_path" in
  "$REPO_ROOT"/*.rs|"$REPO_ROOT"/**/*.rs) ;;
  *) printf '{"decision":"allow"}\n'; exit 0 ;;
esac

# Skip repeated checks within a 30-second window: if the last successful
# xtask check completed less than 30s ago, the workspace hasn't had time
# to accumulate new errors worth re-checking.
STAMP="/tmp/.rustos_xtask_ok"
now=$(date +%s)
if [[ -f "$STAMP" ]]; then
  last=$(cat "$STAMP" 2>/dev/null || echo 0)
  if (( now - last < 30 )); then
    jq -n '{decision:"allow", message:"cargo xtask check: skipped (last ok <30s ago)"}'
    exit 0
  fi
fi

cd "$REPO_ROOT"
log="$(mktemp)"
if timeout 90 cargo xtask check >"$log" 2>&1; then
  rm -f "$log"
  date +%s >"$STAMP"
  jq -n '{decision:"allow", message:"cargo xtask check: ok"}'
  exit 0
fi

tail="$(tail -n 40 "$log")"
rm -f "$log"
jq -n --arg m "cargo xtask check failed (tail):
$tail" '{decision:"allow", message:$m}'
