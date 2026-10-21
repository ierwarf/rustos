#!/usr/bin/env bash
# Claude Code PreToolUse hook for source-navigation/read tools.
# Blocks only genuinely dangerous reads: raw binary artifacts, whole-file reads
# of log/build/vendor token bombs, and whole-file reads of text files larger
# than RUSTOS_HOOK_MAX_WHOLE_READ_BYTES. Symbol navigation, searches, ranged
# reads and MCP answer sizes are never limited.

set -euo pipefail

INPUT="$(cat)"
mapfile -d '' -t fields < <(printf '%s' "$INPUT" | jq -jr '
  (.tool_input.file_path // .tool_input.path // .tool_input.relative_path //
   .arguments.file_path // .arguments.path // .arguments.relative_path //
   .params.file_path // .params.path // .params.relative_path // ""), "\u0000",
  (.tool_input.start_line // .arguments.start_line // .params.start_line //
   .tool_input.offset // .arguments.offset // .params.offset // "" | tostring), "\u0000",
  (.tool_input.end_line // .arguments.end_line // .params.end_line // "" | tostring), "\u0000",
  (.tool_input.limit // .arguments.limit // .params.limit // "" | tostring), "\u0000"
' 2>/dev/null || true)

path="${fields[0]:-}"
range_start="${fields[1]:-}"
range_end="${fields[2]:-}"
range_limit="${fields[3]:-}"

max_whole_read_bytes="${RUSTOS_HOOK_MAX_WHOLE_READ_BYTES:-1048576}"
[[ "$max_whole_read_bytes" =~ ^[0-9]+$ ]] || max_whole_read_bytes=1048576

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

[[ -z "$path" ]] && exit 0

# Any explicit range or limit makes the read bounded.
bounded=0
if [[ "$range_start" =~ ^[0-9]+$ && "$range_end" =~ ^[0-9]+$ ]] || [[ "$range_limit" =~ ^[0-9]+$ ]]; then
  bounded=1
fi

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
case "$path" in
  "$REPO_ROOT"/*) rel_path="${path#"$REPO_ROOT"/}" ;;
  ./*) rel_path="${path#./}" ;;
  *) rel_path="$path" ;;
esac

case "$rel_path" in
  *.pcap|*.bin|*.iso|*.img|perf.data|*/perf.data)
    deny "Binary read blocked: $rel_path. Inspect metadata or use its verification command."
    ;;
esac

(( bounded == 1 )) && exit 0

# Symbol navigation and search tools take a path as a scope, not a whole-file read.
tool_name="$(printf '%s' "$INPUT" | jq -r '.tool_name // ""' 2>/dev/null || true)"
case "$tool_name" in
  mcp__serena__read_file|Read|"") ;;
  *) exit 0 ;;
esac

case "$rel_path" in
  logs/*|*/logs/*|target/*|*/target/*|build/*|*/build/*|vendor/*|*/vendor/*|Cargo.lock|*/Cargo.lock)
    deny "Whole-file read blocked: $rel_path. Search first or request a line range."
    ;;
esac

case "$path" in
  /*) fs_path="$path" ;;
  *) fs_path="$REPO_ROOT/$path" ;;
esac
[[ -f "$fs_path" ]] || exit 0

size="$(stat -c%s -- "$fs_path" 2>/dev/null || echo 0)"
if (( size > max_whole_read_bytes )); then
  deny "Whole-file read blocked: $rel_path (${size} bytes > ${max_whole_read_bytes}). Search first or request a line range."
fi

exit 0
