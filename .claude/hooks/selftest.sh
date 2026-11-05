#!/usr/bin/env bash
# Focused behavior tests for the RustOS Claude Code hooks.

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

expect_deny "destructive shell command is denied" .claude/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'git reset --hard' '{tool_input:{cmd:$cmd}}')"
# Bypass flags are denied when a git command carries them, but not when they only
# appear as text in a file body, a heredoc, or an unrelated command.
for command in 'rm -rf /tmp/cache services' 'rm -fr services' 'rm -r -f services' '/bin/rm --force --recursive services' '(rm -rf services)' 'echo $(rm -rf services)' 'echo `rm -rf services`' "bash -c 'rm -rf services'"; do
  expect_deny "recursive force removal is denied: $command" .claude/hooks/pre_bash_destructive.sh \
    "$(jq -n --arg cmd "$command" '{tool_input:{cmd:$cmd}}')"
done
expect_quiet_allow "separate removals do not share options" .claude/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'rm -r /tmp/old-cache; rm -f /tmp/note.log' '{tool_input:{cmd:$cmd}}')"
expect_quiet_allow "nonrecursive temporary cleanup is allowed" .claude/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'rm -f /tmp/rustos-hook-test.log' '{tool_input:{cmd:$cmd}}')"
expect_deny "git commit --no-verify is denied" .claude/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'git commit --no-verify -m x' '{tool_input:{cmd:$cmd}}')"
expect_deny "git commit without gpg signing is denied" .claude/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'git -C . commit --no-gpg-sign -m x' '{tool_input:{cmd:$cmd}}')"
expect_deny "git push with a hook bypass in a chain is denied" .claude/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'cargo xtask check && git push --no-verify' '{tool_input:{cmd:$cmd}}')"
expect_quiet_allow "bypass flag text in a heredoc body is allowed" .claude/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd $'cat > /tmp/note.txt <<\'EOF\'\nNever pass --no-verify to git commit.\nEOF' '{tool_input:{cmd:$cmd}}')"
# Writes into generated paths are denied per command, not per whole command line.
expect_deny "cp into build is denied" .claude/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'cp a.img build/a.img' '{tool_input:{cmd:$cmd}}')"
expect_deny "tee into logs after a pipe is denied" .claude/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'cargo test | tee logs/test.log' '{tool_input:{cmd:$cmd}}')"
expect_deny "redirection into build is still denied" .claude/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'echo x > build/a.log' '{tool_input:{cmd:$cmd}}')"
expect_quiet_allow "a Rust match arrow before a protected word is not a redirection" .claude/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'echo "IPC_SERVICE_INPUTD => target == NETD"' '{tool_input:{cmd:$cmd}}')"
expect_deny "git push --force is still denied" .claude/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'git push --force origin master' '{tool_input:{cmd:$cmd}}')"
expect_quiet_allow "read-only search is allowed" .claude/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'rg -n "rm -rf" .claude/hooks' '{tool_input:{cmd:$cmd}}')"
expect_quiet_allow "large AI document cat is allowed" .claude/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'cat docs/ai/commands.md' '{tool_input:{cmd:$cmd}}')"

expect_deny "whole Cargo.lock read is denied" .claude/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{relative_path:"Cargo.lock"}}')"
expect_quiet_allow "bounded Cargo.lock read is allowed" .claude/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{relative_path:"Cargo.lock",start_line:10,end_line:40}}')"
expect_deny "zero read limit is not bounded" .claude/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{relative_path:"Cargo.lock",limit:0}}')"
expect_deny "offset alone is not bounded" .claude/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{relative_path:"Cargo.lock",offset:10}}')"
expect_deny "reversed read range is not bounded" .claude/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{relative_path:"Cargo.lock",start_line:40,end_line:10}}')"
expect_deny "traversal does not bypass whole-file guard" .claude/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{relative_path:"docs/../Cargo.lock"}}')"
expect_deny "relative reads use event working directory" .claude/hooks/pre_read_large_file.sh \
  "$(jq -n --arg cwd "$PWD/docs" '{cwd:$cwd,tool_input:{file_path:"../Cargo.lock"}}')"
expect_quiet_allow "Serena read paths stay project-relative with nested cwd" .claude/hooks/pre_read_large_file.sh \
  "$(jq -n --arg cwd "$PWD/build" '{cwd:$cwd,tool_name:"mcp__serena__read_file",tool_input:{relative_path:"docs/ai/commands.md"}}')"
expect_deny "Serena generated read stays guarded with nested cwd" .claude/hooks/pre_read_large_file.sh \
  "$(jq -n --arg cwd "$PWD/docs" '{cwd:$cwd,tool_name:"mcp__serena__read_file",tool_input:{relative_path:"Cargo.lock"}}')"
expect_quiet_allow "whole mid-size document read is allowed" .claude/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{relative_path:"docs/ai/commands.md"}}')"
expect_quiet_allow "large Serena answer is allowed" .claude/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_name:"mcp__serena__find_symbol",tool_input:{max_answer_chars:240000}}')"
expect_deny "binary read is denied" .claude/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{file_path:"build/disk.img"}}')"
expect_quiet_allow "symbol search accepts a binary path scope" .claude/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_name:"mcp__serena__find_symbol",tool_input:{relative_path:"build/disk.img"}}')"

scratch="$(mktemp -d)"
trap 'rm -rf -- "$scratch"' EXIT
ln -s "$PWD/Cargo.lock" "$scratch/lock-link"
expect_deny "symlink does not bypass whole-file guard" .claude/hooks/pre_read_large_file.sh \
  "$(jq -n --arg path "$scratch/lock-link" '{tool_input:{file_path:$path}}')"

# Shared diagnostics must stay small even when the first error is one long line.
source .agents/hooks/lib.sh
printf 'error: %06000d\n' 0 >"$scratch/failure.log"
out="$(rustos_compact_failure 'test gate' 1 "$scratch/failure.log")"
(( ${#out} <= 3072 )) || { echo 'not ok - bounded long diagnostic' >&2; exit 1; }
pass "first diagnostic plus tail is bounded"
printf 'error: unique-short-error\n' >"$scratch/failure.log"
out="$(rustos_compact_failure 'test gate' 1 "$scratch/failure.log")"
[[ "$(printf '%s\n' "$out" | grep -c unique-short-error)" == 1 ]] || {
  echo 'not ok - duplicate diagnostic suppression' >&2; exit 1;
}
pass "first diagnostic is not duplicated in the tail"

out="$(jq -n --arg command $'*** Begin Patch\n*** Update File: Cargo.lock\n@@\n-a\n+b\n*** End Patch' '{tool_input:{command:$command}}' | .claude/hooks/pre_edit_policy.sh)"
if printf '%s' "$out" | jq -e '(.systemMessage != null) and (.decision == null)' >/dev/null; then
  pass "Cargo.lock edit only warns"
else
  printf 'not ok - Cargo.lock edit only warns\n' >&2
  exit 1
fi
out="$(jq -n --arg command $'*** Begin Patch\n*** Update File: Cargo.lock\n@@\n-a\n+b\n*** Update File: build/output.txt\n@@\n-a\n+b\n*** End Patch' '{tool_input:{command:$command}}' | .claude/hooks/pre_edit_policy.sh)"
if printf '%s' "$out" | jq -se 'length == 1 and (.[0].systemMessage | contains("Cargo.lock") and contains("build/output.txt"))' >/dev/null; then
  pass "multi-file edit warning is one JSON document"
else
  echo 'not ok - multi-file edit warning JSON' >&2
  exit 1
fi
out="$(jq -n --arg cwd "$PWD/docs" '{cwd:$cwd,tool_input:{file_path:"../Cargo.lock"}}' | .claude/hooks/pre_edit_policy.sh)"
if printf '%s' "$out" | jq -e '.systemMessage | contains("Cargo.lock")' >/dev/null; then
  pass "edit warning resolves event working directory"
else
  echo 'not ok - relative generated edit warning' >&2
  exit 1
fi
expect_quiet_allow "Serena edit paths stay project-relative with nested cwd" .claude/hooks/pre_edit_policy.sh \
  "$(jq -n --arg cwd "$PWD/build" '{cwd:$cwd,tool_name:"mcp__serena__replace_content",tool_input:{relative_path:"services/netd/src/main.rs"}}')"
out="$(jq -n --arg cwd "$PWD/docs" '{cwd:$cwd,tool_name:"mcp__serena__replace_content",tool_input:{relative_path:"Cargo.lock"}}' | .claude/hooks/pre_edit_policy.sh)"
if printf '%s' "$out" | jq -e '.systemMessage | contains("Cargo.lock")' >/dev/null; then
  pass "Serena generated edit stays advisory with nested cwd"
else
  echo 'not ok - Serena generated edit warning' >&2
  exit 1
fi
expect_quiet_allow "normal Rust edit is allowed" .claude/hooks/pre_edit_policy.sh \
  "$(jq -n --arg command $'*** Begin Patch\n*** Update File: services/initd/src/main.rs\n@@\n-a\n+b\n*** End Patch' '{tool_input:{command:$command}}')"
expect_deny "obvious prompt token is denied" .claude/hooks/user_prompt_policy.sh \
  "$(jq -n --arg prompt 'token sk-testtesttesttesttesttest1234567890' '{prompt:$prompt}')"

printf 'RustOS Claude Code hook selftest passed\n'
