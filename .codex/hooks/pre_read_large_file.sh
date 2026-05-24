#!/usr/bin/env bash
# Codex PreToolUse hook for Read/file-access tools.
#
# Reads a JSON event from stdin describing the pending tool call and
# writes a JSON decision to stdout. Blocks reads of large logs, build
# artifacts, and other token-bombs that should be filtered with rg/tail
# instead.
#
# Wired in .codex/config.toml (project-scoped) under [[hooks.PreToolUse]]
# with a matcher that targets the Read tool (and any file-reading variants).

set -euo pipefail

INPUT="$(cat)"

# Extract the candidate path. Codex's exact event schema can vary across
# versions, so probe the common shapes and fall back to allow.
path="$(printf '%s' "$INPUT" | jq -r '
  .tool_input.file_path // .tool_input.path //
  .tool_input.relative_path //
  .arguments.file_path // .arguments.path //
  .arguments.relative_path //
  .params.file_path  // .params.path  //
  .params.relative_path //
  empty
' 2>/dev/null || true)"

if [[ -z "$path" ]]; then
  exit 0
fi

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
case "$path" in
  "$REPO_ROOT"/*) rel_path="${path#"$REPO_ROOT"/}" ;;
  ./*) rel_path="${path#./}" ;;
  *) rel_path="$path" ;;
esac

# Hard-block extensions / directories regardless of size.
case "$rel_path" in
  logs/*.log|logs/*.txt|*/logs/*.log|*/logs/*.txt|*.pcap|*.bin|*.iso|*.img|perf.data|*/perf.data|target/*|*/target/*|build/*|*/build/*|vendor/*|*/vendor/*|Cargo.lock|*/Cargo.lock)
    msg="Blocked: $rel_path is in the do-not-inspect set (logs/target/build/vendor/Cargo.lock/binary). \
Use rg, tail -n, or sed -n line ranges instead. \
See AGENTS.md > Do Not Inspect By Default."
    jq -n --arg m "$msg" '{
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "deny",
        permissionDecisionReason: $m
      },
      decision: "block",
      reason: $m
    }'
    exit 0
    ;;
esac

case "$path" in
  /*) fs_path="$path" ;;
  *) fs_path="$REPO_ROOT/$path" ;;
esac

if [[ ! -e "$fs_path" ]]; then
  exit 0
fi

# Size gate for everything else: 256KB soft limit.
size=$(stat -c%s -- "$fs_path" 2>/dev/null || echo 0)
if (( size > 262144 )); then
  msg="Blocked: $rel_path is ${size} bytes (>256KB). \
Use rg/tail/sed line ranges, or summarize via the log-summarizer subagent. \
If you genuinely need a full read, ask the user first."
  jq -n --arg m "$msg" '{
    hookSpecificOutput: {
      hookEventName: "PreToolUse",
      permissionDecision: "deny",
      permissionDecisionReason: $m
    },
    decision: "block",
    reason: $m
  }'
  exit 0
fi

exit 0
