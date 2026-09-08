#!/usr/bin/env bash
# Codex PreToolUse hook for Read/file-access tools.
# Blocks whole-file token bombs while allowing an explicitly bounded text read.

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
[[ -z "$path" ]] && exit 0

max_lines="${RUSTOS_HOOK_MAX_READ_LINES:-200}"
[[ "$max_lines" =~ ^[0-9]+$ ]] || max_lines=200
bounded=0
if [[ "$range_start" =~ ^[0-9]+$ && "$range_end" =~ ^[0-9]+$ ]] \
  && (( range_end >= range_start && range_end - range_start < max_lines )); then
  bounded=1
elif [[ "$range_limit" =~ ^[0-9]+$ ]] && (( range_limit > 0 && range_limit <= max_lines )); then
  bounded=1
fi

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
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

case "$rel_path" in
  *.pcap|*.bin|*.iso|*.img|perf.data|*/perf.data)
    deny "Binary read blocked: $rel_path. Inspect metadata or use its verification command."
    ;;
esac

case "$rel_path" in
  logs/*|*/logs/*|target/*|*/target/*|build/*|*/build/*|vendor/*|*/vendor/*|Cargo.lock|*/Cargo.lock)
    (( bounded == 1 )) && exit 0
    deny "Whole-file read blocked: $rel_path. Search first or request <=${max_lines} focused lines."
    ;;
esac

case "$path" in
  /*) fs_path="$path" ;;
  *) fs_path="$REPO_ROOT/$path" ;;
esac
[[ -e "$fs_path" ]] || exit 0

size="$(stat -c%s -- "$fs_path" 2>/dev/null || echo 0)"
if (( size > 262144 && bounded == 0 )); then
  deny "Large read blocked: $rel_path (${size} bytes). Search first or request <=${max_lines} focused lines."
fi

exit 0
