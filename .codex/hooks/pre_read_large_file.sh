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
  .arguments.file_path // .arguments.path //
  .params.file_path  // .params.path  //
  empty
' 2>/dev/null || true)"

if [[ -z "$path" || ! -e "$path" ]]; then
  printf '{"decision":"allow"}\n'
  exit 0
fi

# Hard-block extensions / directories regardless of size.
case "$path" in
  */logs/*.log|*/logs/*.txt|*.pcap|*.bin|*.iso|*.img|*/perf.data|*/target/*|*/build/*|*/vendor/*)
    msg="Blocked: $path is in the do-not-inspect set (logs/target/build/vendor/binary). \
Use rg, tail -n, or sed -n line ranges instead. \
See AGENTS.md > Do Not Inspect By Default."
    jq -n --arg m "$msg" '{decision:"block", message:$m}'
    exit 0
    ;;
esac

# Size gate for everything else: 256KB soft limit.
size=$(stat -c%s -- "$path" 2>/dev/null || echo 0)
if (( size > 262144 )); then
  msg="Blocked: $path is ${size} bytes (>256KB). \
Use rg/tail/sed line ranges, or summarize via the log-summarizer subagent. \
If you genuinely need a full read, ask the user first."
  jq -n --arg m "$msg" '{decision:"block", message:$m}'
  exit 0
fi

printf '{"decision":"allow"}\n'
