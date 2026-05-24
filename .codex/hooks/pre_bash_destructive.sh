#!/usr/bin/env bash
# Codex PreToolUse hook for the Bash tool.
#
# Blocks the most common destructive shell patterns and asks the user to
# re-issue with explicit intent. This is a safety net, not a substitute
# for sandbox_mode or approval_policy.

set -euo pipefail

INPUT="$(cat)"

cmd="$(printf '%s' "$INPUT" | jq -r '
  .tool_input.command // .arguments.command // .params.command // empty
' 2>/dev/null || true)"

if [[ -z "$cmd" ]]; then
  exit 0
fi

trimmed_cmd="${cmd#"${cmd%%[![:space:]]*}"}"

block() {
  local reason="$1"
  jq -n --arg m "Blocked destructive command: $reason. \
Re-issue only if the user has explicitly authorized it." \
    '{
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

write_protected_path_re='(^|[[:space:];|&>])(\./)?(build|target|vendor|logs)(/|[[:space:]]|$)|(^|[[:space:];|&>])(\./)?Cargo\.lock([[:space:]]|$)|(^|[[:space:];|&>])(\./)?perf\.data([[:space:]]|$)'

# Read-only searches often include dangerous command text as the query. Let
# those through unless the command chains into another shell action.
if [[ "$trimmed_cmd" =~ ^(rg|grep|git[[:space:]]+grep)[[:space:]] ]] \
  && [[ "$trimmed_cmd" != *";"* ]] \
  && [[ "$trimmed_cmd" != *"&"* ]] \
  && [[ "$trimmed_cmd" != *"|"* ]] \
  && [[ "$trimmed_cmd" != *'`'* ]] \
  && [[ "$trimmed_cmd" != *'$('* ]]; then
  exit 0
fi

# rm -rf on anything outside /tmp
if [[ "$cmd" =~ rm[[:space:]]+-[a-zA-Z]*r[a-zA-Z]*f ]] && [[ ! "$cmd" =~ /tmp/ ]]; then
  block "rm -rf outside /tmp"
fi

# Direct shell writes into generated/vendor/log paths. Build tools such as
# cargo/xtask are allowed to write these trees through their own commands.
if [[ "$cmd" =~ (^[[:space:]]*|[[:space:];|&])[[:alnum:]_./-]*(cp|mv|install|rsync|tee|truncate|sed)[[:space:]] ]] \
  && [[ "$cmd" =~ $write_protected_path_re ]]; then
  block "direct shell write to generated/vendor/log path"
fi
if [[ "$cmd" =~ (>|>>)[[:space:]]*(\./)?(build|target|vendor|logs|Cargo\.lock|perf\.data)(/|[[:space:]]|$) ]]; then
  block "redirection into generated/vendor/log path"
fi

# git destructive ops
if [[ "$cmd" =~ git[[:space:]]+reset[[:space:]]+--hard ]]; then
  block "git reset --hard"
fi
if [[ "$cmd" =~ git[[:space:]]+push[[:space:]].*--force ]] || [[ "$cmd" =~ git[[:space:]]+push[[:space:]].*-f([[:space:]]|$) ]]; then
  block "git push --force"
fi
if [[ "$cmd" =~ git[[:space:]]+clean[[:space:]]+-[a-zA-Z]*f ]]; then
  block "git clean -f"
fi
if [[ "$cmd" =~ git[[:space:]]+branch[[:space:]]+-D ]]; then
  block "git branch -D"
fi
if [[ "$cmd" =~ git[[:space:]]+checkout[[:space:]]+\. ]] || [[ "$cmd" =~ git[[:space:]]+restore[[:space:]]+\. ]]; then
  block "git checkout/restore . (mass discard)"
fi
if [[ "$cmd" =~ git[[:space:]]+(checkout|restore)[[:space:]].*(AGENTS\.md|docs/ai/|\.codex/) ]]; then
  block "discarding agent policy/hook files"
fi

# Hook / signing bypass
if [[ "$cmd" =~ --no-verify ]] || [[ "$cmd" =~ --no-gpg-sign ]]; then
  block "skipping hooks or signing"
fi
if [[ "$cmd" =~ --dangerously-bypass-hook-trust ]] || [[ "$cmd" =~ --dangerously-bypass-approvals-and-sandbox ]]; then
  block "bypassing Codex hook trust or sandbox controls"
fi

# Filesystem nukes
if [[ "$cmd" =~ ^[[:space:]]*(sudo[[:space:]]+)?(dd|mkfs|fdisk|parted|wipefs) ]]; then
  block "raw disk command"
fi

exit 0
