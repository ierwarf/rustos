#!/usr/bin/env bash
# Codex PreToolUse shell policy.
#
# One hook handles both destructive-command blocking and conditional pre-commit
# gates. Keeping these in one process avoids paying two jq/bash startups for
# every harmless shell command.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
# shellcheck source=.agents/hooks/lib.sh
source "$SCRIPT_DIR/../../.agents/hooks/lib.sh"

INPUT="$(cat)"
cmd="$(printf '%s' "$INPUT" | jq -r '
  .tool_input.command // .tool_input.cmd //
  .arguments.command // .arguments.cmd //
  .params.command // .params.cmd // empty
' 2>/dev/null || true)"

[[ -z "$cmd" ]] && exit 0
trimmed_cmd="${cmd#"${cmd%%[![:space:]]*}"}"

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

block_destructive() {
  block "Blocked destructive command: $1. Re-issue only if the user explicitly authorized it."
}

write_protected_path_re='(^|[[:space:];|&>])(\./)?(build|target|vendor|logs)(/|[[:space:]]|$)|(^|[[:space:];|&>])(\./)?Cargo\.lock([[:space:]]|$)|(^|[[:space:];|&>])(\./)?perf\.data([[:space:]]|$)'

# Pure searches may contain dangerous text as their query. Avoid false blocks
# unless the command chains into another shell action.
if [[ "$trimmed_cmd" =~ ^(rg|grep|git[[:space:]]+grep)[[:space:]] ]] \
  && [[ "$trimmed_cmd" != *";"* ]] \
  && [[ "$trimmed_cmd" != *"&"* ]] \
  && [[ "$trimmed_cmd" != *"|"* ]] \
  && [[ "$trimmed_cmd" != *'`'* ]] \
  && [[ "$trimmed_cmd" != *'$('* ]]; then
  exit 0
fi

if [[ "$cmd" =~ rm[[:space:]]+-[a-zA-Z]*r[a-zA-Z]*f ]] && [[ ! "$cmd" =~ /tmp/ ]]; then
  block_destructive "rm -rf outside /tmp"
fi
if [[ "$cmd" =~ (^[[:space:]]*|[[:space:];|&])[[:alnum:]_./-]*(cp|mv|install|rsync|tee|truncate)[[:space:]] ]] \
  && [[ "$cmd" =~ $write_protected_path_re ]]; then
  block_destructive "direct shell write to generated/vendor/log path"
fi
if [[ "$cmd" =~ (^[[:space:]]*|[[:space:];|&])sed[[:space:]]+(-[^[:space:]]*i[^[:space:]]*|--in-place([=[:space:]]|$)) ]] \
  && [[ "$cmd" =~ $write_protected_path_re ]]; then
  block_destructive "in-place sed write to generated/vendor/log path"
fi
if [[ "$cmd" =~ (>|>>)[[:space:]]*(\./)?(build|target|vendor|logs|Cargo\.lock|perf\.data)(/|[[:space:]]|$) ]]; then
  block_destructive "redirection into generated/vendor/log path"
fi

if [[ "$cmd" =~ git[[:space:]]+reset[[:space:]]+--hard ]]; then
  block_destructive "git reset --hard"
fi
if [[ "$cmd" =~ git[[:space:]]+push[[:space:]].*--force ]] || [[ "$cmd" =~ git[[:space:]]+push[[:space:]].*-f([[:space:]]|$) ]]; then
  block_destructive "git push --force"
fi
if [[ "$cmd" =~ git[[:space:]]+clean[[:space:]]+-[a-zA-Z]*f ]]; then
  block_destructive "git clean -f"
fi
if [[ "$cmd" =~ git[[:space:]]+branch[[:space:]]+-D ]]; then
  block_destructive "git branch -D"
fi
if [[ "$cmd" =~ git[[:space:]]+checkout[[:space:]]+\. ]] || [[ "$cmd" =~ git[[:space:]]+restore[[:space:]]+\. ]]; then
  block_destructive "git checkout/restore . (mass discard)"
fi
if [[ "$cmd" =~ git[[:space:]]+(checkout|restore)[[:space:]].*(AGENTS\.md|docs/ai/|\.codex/) ]]; then
  block_destructive "discarding agent policy/hook files"
fi

if [[ "$cmd" =~ --no-verify ]] || [[ "$cmd" =~ --no-gpg-sign ]]; then
  block_destructive "skipping hooks or signing"
fi
if [[ "$cmd" =~ --dangerously-bypass-hook-trust ]] || [[ "$cmd" =~ --dangerously-bypass-approvals-and-sandbox ]]; then
  block_destructive "bypassing Codex hook trust or sandbox controls"
fi
if [[ "$cmd" =~ ^[[:space:]]*(sudo[[:space:]]+)?(dd|mkfs|fdisk|parted|wipefs) ]]; then
  block_destructive "raw disk command"
fi

# Everything below is commit-only. Ordinary shell calls return after one JSON
# parse and the cheap safety regexes above.
if [[ ! "$cmd" =~ git[[:space:]]+commit([[:space:]]|$) ]]; then
  exit 0
fi

REPO_ROOT="$(rustos_repo_root)"
cd "$REPO_ROOT" 2>/dev/null || exit 0
staged="$(git diff --cached --name-only --diff-filter=ACMR 2>/dev/null || true)"
[[ -z "$staged" ]] && exit 0

log="$(mktemp)"
trap 'rm -f "$log"' EXIT

run_gate() {
  local label="$1"
  local seconds="$2"
  shift 2
  local rc
  if rustos_run_bounded "$seconds" "$log" "$@"; then
    return 0
  else
    rc=$?
  fi
  block "$(rustos_compact_failure "$label" "$rc" "$log")"
}

if printf '%s\n' "$staged" | grep -Eq '\.rs$'; then
  run_gate "cargo fmt --check" 20 cargo fmt --all -- --check
fi

# The hook bundle/formal registry selftest is relevant only when agent policy or
# its owned validation infrastructure is actually part of the commit.
if printf '%s\n' "$staged" | grep -Eq '^(AGENTS\.md|\.codex/|\.claude/|\.agents/|docs/ai/|tools/agent/|formal/)'; then
  run_gate "agent hook selftest" 25 .codex/hooks/selftest.sh
fi

# Post-edit checks may be intentionally coalesced. Before a source/build-system
# commit, validate the exact current workspace fingerprint unless that same
# state already passed. Documentation/agent-only commits avoid this cost.
if printf '%s\n' "$staged" | grep -Eq '(^|/)(Cargo\.toml|RUSTOS\.package\.toml)$|\.rs$|^(kernel|services|libs|apps|compat|boot|tools/xtask|driver-domains)/'; then
  fingerprint="$(rustos_workspace_fingerprint "$REPO_ROOT")"
  if [[ "$(rustos_read_check_stamp "$REPO_ROOT")" != "$fingerprint" ]]; then
    run_gate "cargo xtask check" 50 cargo xtask check
    rustos_write_check_stamp "$REPO_ROOT" "$fingerprint"
  fi
fi

exit 0
