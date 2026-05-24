#!/usr/bin/env bash
# Codex PreToolUse hook for apply_patch/Edit/Write.
# Protects generated/vendor/token-bomb paths from accidental agent edits and
# forces broad edits to be split into reviewable slices.

set -euo pipefail

INPUT="$(cat)"

payload="$(printf '%s' "$INPUT" | jq -r '
  .tool_input.command // .tool_input.file_path // .tool_input.path //
  .arguments.command // .arguments.file_path // .arguments.path //
  .params.command // .params.file_path // .params.path //
  empty
' 2>/dev/null || true)"

[[ -z "$payload" ]] && exit 0

block() {
  local reason="$1"
  jq -n --arg m "$reason" '{
    hookSpecificOutput: {
      hookEventName: "PreToolUse",
      permissionDecision: "deny",
      permissionDecisionReason: $m
    },
    decision: "block",
    reason: $m
  }'
  exit 0
}

paths="$(
  {
    printf '%s\n' "$payload" | sed -nE 's/^\*\*\* (Add|Update|Delete) File: (.+)$/\2/p'
    printf '%s\n' "$payload" | sed -nE 's/^\*\*\* Move to: (.+)$/\1/p'
    if [[ "$payload" != *$'\n'* ]]; then
      printf '%s\n' "$payload"
    fi
  } | sed 's#^\./##' | sed '/^$/d' | sort -u
)"

[[ -z "$paths" ]] && exit 0

count="$(printf '%s\n' "$paths" | sed '/^$/d' | wc -l)"
if (( count > 20 )); then
  block "Blocked broad edit touching ${count} files. Split the change or ask the user to approve the broad edit."
fi

while IFS= read -r path; do
  case "$path" in
    logs/*|target/*|build/*|vendor/*|Cargo.lock|perf.data|*.pcap|*.bin|*.iso|*.img)
      block "Blocked edit to protected RustOS path: $path. Use generated/build commands or focused tooling instead."
      ;;
  esac
done <<< "$paths"

exit 0
