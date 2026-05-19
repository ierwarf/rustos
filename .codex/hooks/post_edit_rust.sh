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

# Only react to *.rs edits inside this workspace.
case "$path" in
  "$REPO_ROOT"/*.rs|"$REPO_ROOT"/**/*.rs) ;;
  *) printf '{"decision":"allow"}\n'; exit 0 ;;
esac

# Run cargo xtask check with a short timeout so a runaway compile cannot
# stall the loop. Capture only the tail of stderr.
cd "$REPO_ROOT"
log="$(mktemp)"
if timeout 90 cargo xtask check >"$log" 2>&1; then
  rm -f "$log"
  jq -n '{decision:"allow", message:"cargo xtask check: ok"}'
  exit 0
fi

tail="$(tail -n 40 "$log" | jq -Rs .)"
rm -f "$log"
printf '{"decision":"allow","message":"cargo xtask check failed (tail):\\n%s"}\n' "${tail//\"/}"
