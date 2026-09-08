#!/usr/bin/env bash
# Codex PreToolUse hook for source-navigation/read tools.
# Blocks whole-file token bombs and oversized MCP discovery payloads while
# allowing focused reads and compact source-navigation queries.

set -euo pipefail

INPUT="$(cat)"
mapfile -d '' -t fields < <(printf '%s' "$INPUT" | jq -jr '
  (.tool_input.file_path // .tool_input.path // .tool_input.relative_path //
   .arguments.file_path // .arguments.path // .arguments.relative_path //
   .params.file_path // .params.path // .params.relative_path // ""), "\u0000",
  (.tool_input.start_line // .arguments.start_line // .params.start_line //
   .tool_input.offset // .arguments.offset // .params.offset // "" | tostring), "\u0000",
  (.tool_input.end_line // .arguments.end_line // .params.end_line // "" | tostring), "\u0000",
  (.tool_input.limit // .arguments.limit // .params.limit // "" | tostring), "\u0000",
  (.tool_name // .name // ""), "\u0000",
  (.tool_input.max_answer_chars // .arguments.max_answer_chars // .params.max_answer_chars // "" | tostring), "\u0000",
  (.tool_input.max_results // .arguments.max_results // .params.max_results // "" | tostring), "\u0000",
  (if ((.tool_input.compact? == false) or (.arguments.compact? == false) or (.params.compact? == false))
   then "false" else "true-or-unset" end), "\u0000"
' 2>/dev/null || true)

path="${fields[0]:-}"
range_start="${fields[1]:-}"
range_end="${fields[2]:-}"
range_limit="${fields[3]:-}"
tool_name="${fields[4]:-}"
max_answer_chars="${fields[5]:-}"
max_results="${fields[6]:-}"
compact_state="${fields[7]:-}"

max_lines="${RUSTOS_HOOK_MAX_READ_LINES:-120}"
max_mcp_chars="${RUSTOS_HOOK_MAX_MCP_ANSWER_CHARS:-8000}"
max_ast_results="${RUSTOS_HOOK_MAX_AST_RESULTS:-24}"
max_codegraph_results="${RUSTOS_HOOK_MAX_CODEGRAPH_RESULTS:-16}"
[[ "$max_lines" =~ ^[0-9]+$ ]] || max_lines=120
[[ "$max_mcp_chars" =~ ^[0-9]+$ ]] || max_mcp_chars=8000
[[ "$max_ast_results" =~ ^[0-9]+$ ]] || max_ast_results=24
[[ "$max_codegraph_results" =~ ^[0-9]+$ ]] || max_codegraph_results=16

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

if [[ "$max_answer_chars" =~ ^[0-9]+$ ]] && (( max_answer_chars > max_mcp_chars )); then
  deny "MCP answer budget blocked: requested ${max_answer_chars} chars; use <=${max_mcp_chars} and narrow the query first."
fi

if [[ "$tool_name" == mcp__ast_grep__* ]] \
  && [[ "$max_results" =~ ^[0-9]+$ ]] \
  && (( max_results > max_ast_results )); then
  deny "ast-grep result budget blocked: requested ${max_results}; use <=${max_ast_results} and tighten the pattern first."
fi

if [[ "$tool_name" == mcp__codegraph__codegraph_* ]] \
  && [[ "$range_limit" =~ ^[0-9]+$ ]] \
  && (( range_limit > max_codegraph_results )); then
  deny "CodeGraph result budget blocked: requested ${range_limit}; use <=${max_codegraph_results} and expand one selected symbol later."
fi

if [[ "$tool_name" == *codegraph_symbol_search* && "$compact_state" == "false" ]]; then
  deny "CodeGraph symbol search must use compact=true; expand only the selected symbol after discovery."
fi

[[ -z "$path" ]] && exit 0

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
