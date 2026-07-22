#!/usr/bin/env bash
# Codex PreToolUse hook for Read/file-access tools.
# Blocks whole-file token bombs while allowing an explicitly bounded text read.

set -euo pipefail

INPUT="$(cat)"

# Codex and MCP adapters use slightly different event shapes. Unknown shapes
# fail open so a hook-schema drift does not disable the development session.
path="$(printf '%s' "$INPUT" | jq -r '
  .tool_input.file_path // .tool_input.path // .tool_input.relative_path //
  .arguments.file_path // .arguments.path // .arguments.relative_path //
  .params.file_path // .params.path // .params.relative_path // empty
' 2>/dev/null || true)"

[[ -z "$path" ]] && exit 0

range_start="$(printf '%s' "$INPUT" | jq -r '
  .tool_input.start_line // .arguments.start_line // .params.start_line //
  .tool_input.offset // .arguments.offset // .params.offset // empty
' 2>/dev/null || true)"
range_end="$(printf '%s' "$INPUT" | jq -r '
  .tool_input.end_line // .arguments.end_line // .params.end_line // empty
' 2>/dev/null || true)"
range_limit="$(printf '%s' "$INPUT" | jq -r '
  .tool_input.limit // .arguments.limit // .params.limit // empty
' 2>/dev/null || true)"

bounded=0
if [[ "$range_start" =~ ^[0-9]+$ && "$range_end" =~ ^[0-9]+$ ]] \
  && (( range_end >= range_start && range_end - range_start < 200 )); then
  bounded=1
elif [[ "$range_limit" =~ ^[0-9]+$ ]] && (( range_limit > 0 && range_limit <= 200 )); then
  bounded=1
fi

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
case "$path" in
  "$REPO_ROOT"/*) rel_path="${path#"$REPO_ROOT"/}" ;;
  ./*) rel_path="${path#./}" ;;
  *) rel_path="$path" ;;
esac

deny() {
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

# Binary artifact reads are never useful model context, even with a range.
case "$rel_path" in
  *.pcap|*.bin|*.iso|*.img|perf.data|*/perf.data)
    deny "Blocked binary read: $rel_path. Inspect metadata or use the owning verification command."
    ;;
esac

# Protected text trees are allowed only when the caller supplied a small range.
case "$rel_path" in
  logs/*|*/logs/*|target/*|*/target/*|build/*|*/build/*|vendor/*|*/vendor/*|Cargo.lock|*/Cargo.lock)
    if (( bounded == 1 )); then
      exit 0
    fi
    deny "Blocked whole-file read of $rel_path. Use rg or request at most 200 focused lines; see docs/ai/token-policy.md."
    ;;
esac

case "$path" in
  /*) fs_path="$path" ;;
  *) fs_path="$REPO_ROOT/$path" ;;
esac

[[ -e "$fs_path" ]] || exit 0

# Large ordinary files follow the same bounded-read escape hatch. No user
# approval is needed for a focused read that already satisfies repository
# token policy.
size="$(stat -c%s -- "$fs_path" 2>/dev/null || echo 0)"
if (( size > 262144 && bounded == 0 )); then
  deny "Blocked whole-file read of $rel_path (${size} bytes). Search first or request at most 200 focused lines."
fi

exit 0
