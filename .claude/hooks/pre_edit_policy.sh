#!/usr/bin/env bash
# Claude Code PreToolUse hook for apply_patch/Edit/Write.
# Never blocks: it only warns when an edit touches a generated/vendor path.

set -euo pipefail

INPUT="$(cat)"

payload="$(printf '%s' "$INPUT" | jq -r '
  .tool_input.command // .tool_input.file_path // .tool_input.path // .tool_input.relative_path //
  .arguments.command // .arguments.file_path // .arguments.path // .arguments.relative_path //
  .params.command // .params.file_path // .params.path // .params.relative_path //
  empty
' 2>/dev/null || true)"

[[ -z "$payload" ]] && exit 0

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

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
edit_cwd="$(printf '%s' "$INPUT" | jq -r '.cwd // empty' 2>/dev/null || true)"
edit_cwd="${edit_cwd:-$REPO_ROOT}"
tool_name="$(printf '%s' "$INPUT" | jq -r '.tool_name // ""' 2>/dev/null || true)"
# Serena relative_path is project-root-relative, unlike native Edit/Write paths.
[[ "$tool_name" == mcp__serena__* ]] && edit_cwd="$REPO_ROOT"
warnings=()
while IFS= read -r path; do
  case "$path" in
    /*) normalized="$(realpath -m -- "$path")" ;;
    *) normalized="$(realpath -m -- "$edit_cwd/$path")" ;;
  esac
  normalized="${normalized#"$REPO_ROOT"/}"
  case "$normalized" in
    logs/*|*/logs/*|target/*|*/target/*|build/*|*/build/*|vendor/*|*/vendor/*|Cargo.lock|*/Cargo.lock|perf.data|*/perf.data|*.pcap|*.bin|*.iso|*.img)
      warnings+=("$path")
      ;;
  esac
done <<< "$paths"

# Hook stdout is a single JSON document, including multi-file patch warnings.
if (( ${#warnings[@]} > 0 )); then
  printf -v message 'Editing generated/vendor path: %s\n' "${warnings[@]}"
  jq -n --arg m "${message%$'\n'}" '{systemMessage:$m}'
fi

exit 0
