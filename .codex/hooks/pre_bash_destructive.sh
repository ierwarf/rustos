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
  printf '{"decision":"allow"}\n'
  exit 0
fi

block() {
  local reason="$1"
  jq -n --arg m "Blocked destructive command: $reason. \
Re-issue only if the user has explicitly authorized it." \
    '{decision:"block", message:$m}'
  exit 0
}

# rm -rf on anything outside /tmp
if [[ "$cmd" =~ rm[[:space:]]+-[a-zA-Z]*r[a-zA-Z]*f ]] && [[ ! "$cmd" =~ /tmp/ ]]; then
  block "rm -rf outside /tmp"
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

# Hook / signing bypass
if [[ "$cmd" =~ --no-verify ]] || [[ "$cmd" =~ --no-gpg-sign ]]; then
  block "skipping hooks or signing"
fi

# Filesystem nukes
if [[ "$cmd" =~ ^[[:space:]]*(sudo[[:space:]]+)?(dd|mkfs|fdisk|parted|wipefs) ]]; then
  block "raw disk command"
fi

printf '{"decision":"allow"}\n'
