#!/usr/bin/env bash
# Shared PostToolUse hook for Rust edits, wired from Codex and Claude.
#
# Keep the per-edit path cheap: validate structural source contracts only.
# Workspace compilation/checks are routed explicitly by `cargo xtask dev-plan`
# while iterating and enforced from the shell pre-commit gate before a source
# commit when the exact workspace fingerprint has not already passed.

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
log="$(mktemp)"
trap 'rm -f "$log"' EXIT

if rustos_run_bounded 8 "$log" python3 formal/check-rust-source-contracts.py; then
  exit 0
fi
rc=$?
message="$(rustos_compact_failure 'source-contract lint' "$rc" "$log")"
jq -n --arg m "$message" '{systemMessage:$m}'
exit 0
