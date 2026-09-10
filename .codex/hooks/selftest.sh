#!/usr/bin/env bash
# Focused behavior tests for the RustOS Codex hooks.

set -euo pipefail
cd "$(dirname "$0")/../.."

pass() { printf 'ok - %s\n' "$1"; }

expect_deny() {
  local name="$1" script="$2" event="$3"
  if printf '%s' "$event" | "$script" | jq -e '(.hookSpecificOutput.permissionDecision == "deny") or (.decision == "block")' >/dev/null; then
    pass "$name"
    return
  fi
  printf 'not ok - %s\n' "$name" >&2
  exit 1
}

expect_quiet_allow() {
  local name="$1" script="$2" event="$3" bytes
  bytes="$(printf '%s' "$event" | "$script" | wc -c)"
  if [[ "$bytes" == "0" ]]; then
    pass "$name"
    return
  fi
  printf 'not ok - %s: expected quiet allow, got %s bytes\n' "$name" "$bytes" >&2
  exit 1
}

test -x tools/agent/check-ai-context-contract.sh || {
  echo 'not ok - AI context contract checker is not executable' >&2
  exit 1
}
tools/agent/check-ai-context-contract.sh >/dev/null
pass "AI context efficiency contract passes"

expect_deny "destructive shell command is denied" .codex/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'git reset --hard' '{tool_input:{cmd:$cmd}}')"
expect_quiet_allow "read-only search is allowed" .codex/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'rg -n "rm -rf" .codex/hooks' '{tool_input:{cmd:$cmd}}')"
expect_deny "oversized shell output is denied" .codex/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'cat docs/ai/commands.md' '{tool_input:{cmd:$cmd,max_output_tokens:12000}}')"
expect_deny "unbudgeted large AI document cat is denied" .codex/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'cat docs/ai/commands.md' '{tool_input:{cmd:$cmd}}')"
expect_quiet_allow "budgeted large AI document read is allowed" .codex/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'sed -n "1,40p" docs/ai/commands.md' '{tool_input:{cmd:$cmd,max_output_tokens:1500}}')"

expect_deny "whole Cargo.lock read is denied" .codex/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{relative_path:"Cargo.lock"}}')"
expect_quiet_allow "bounded Cargo.lock read is allowed" .codex/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{relative_path:"Cargo.lock",start_line:10,end_line:40}}')"
expect_deny "whole large AI document read is denied" .codex/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{relative_path:"docs/ai/commands.md"}}')"
expect_quiet_allow "focused large AI document range is allowed" .codex/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{relative_path:"docs/ai/commands.md",start_line:1,end_line:40}}')"
expect_deny "oversized Serena answer is denied" .codex/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_name:"mcp__serena__find_symbol",tool_input:{max_answer_chars:24000}}')"
expect_quiet_allow "bounded Serena answer is allowed" .codex/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_name:"mcp__serena__find_symbol",tool_input:{max_answer_chars:8000}}')"

expect_deny "Cargo.lock edit is denied" .codex/hooks/pre_edit_policy.sh \
  "$(jq -n --arg command $'*** Begin Patch\n*** Update File: Cargo.lock\n@@\n-a\n+b\n*** End Patch' '{tool_input:{command:$command}}')"
expect_quiet_allow "normal Rust edit is allowed" .codex/hooks/pre_edit_policy.sh \
  "$(jq -n --arg command $'*** Begin Patch\n*** Update File: services/initd/src/main.rs\n@@\n-a\n+b\n*** End Patch' '{tool_input:{command:$command}}')"
expect_deny "obvious prompt token is denied" .codex/hooks/user_prompt_policy.sh \
  "$(jq -n --arg prompt 'token sk-testtesttesttesttesttest1234567890' '{prompt:$prompt}')"

printf 'RustOS Codex hook selftest passed\n'
