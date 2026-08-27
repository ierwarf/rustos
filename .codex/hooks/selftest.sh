#!/usr/bin/env bash
# Fast local verification for the RustOS Codex hook bundle.

set -euo pipefail

cd "$(dirname "$0")/../.."

pass() {
  printf 'ok - %s\n' "$1"
}

expect_deny() {
  local name="$1"
  local script="$2"
  local event="$3"

  if printf '%s' "$event" | "$script" | jq -e '
    (.hookSpecificOutput.permissionDecision == "deny")
    or (.decision == "block")
  ' >/dev/null; then
    pass "$name"
    return
  fi

  printf 'not ok - %s\n' "$name" >&2
  exit 1
}

expect_quiet_allow() {
  local name="$1"
  local script="$2"
  local event="$3"
  local bytes

  bytes="$(printf '%s' "$event" | "$script" | wc -c)"
  if [[ "$bytes" == "0" ]]; then
    pass "$name"
    return
  fi

  printf 'not ok - %s: expected quiet allow, got %s bytes\n' "$name" "$bytes" >&2
  exit 1
}

expect_match() {
  local name="$1"
  local file="$2"
  local pattern="$3"

  if grep -Eq -- "$pattern" "$file"; then
    pass "$name"
    return
  fi

  printf 'not ok - %s: %s lacks required contract\n' "$name" "$file" >&2
  exit 1
}

expect_no_match() {
  local name="$1"
  local file="$2"
  local pattern="$3"

  if grep -Eq -- "$pattern" "$file"; then
    printf 'not ok - %s: %s contains forbidden drift\n' "$name" "$file" >&2
    exit 1
  fi

  pass "$name"
}

expect_deny \
  "destructive rm is denied" \
  .codex/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg command 'rm -rf hook_probe_nonexistent' '{tool_input:{command:$command}}')"

expect_deny \
  "unified exec destructive command is denied" \
  .codex/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'git reset --hard' '{tool_input:{cmd:$cmd}}')"

expect_quiet_allow \
  "read-only search for dangerous text is allowed" \
  .codex/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg command 'rg -n "rm -rf" .codex/hooks' '{tool_input:{command:$command}}')"

expect_quiet_allow \
  "read-only generated-path discovery is allowed" \
  .codex/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg cmd 'find . -name target -print | sed -n "1,20p"' '{tool_input:{cmd:$cmd}}')"

expect_deny \
  "whole Cargo.lock read is denied" \
  .codex/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{relative_path:"Cargo.lock"}}')"

expect_quiet_allow \
  "bounded Cargo.lock read is allowed" \
  .codex/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{relative_path:"Cargo.lock",start_line:10,end_line:40}}')"

expect_deny \
  "bounded binary read is denied" \
  .codex/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{relative_path:"build/probe.bin",start_line:0,end_line:10}}')"

expect_deny \
  "Cargo.lock edit is denied" \
  .codex/hooks/pre_edit_policy.sh \
  "$(jq -n --arg command $'*** Begin Patch\n*** Update File: Cargo.lock\n@@\n-a\n+b\n*** End Patch' '{tool_input:{command:$command}}')"

expect_quiet_allow \
  "normal Rust edit is allowed" \
  .codex/hooks/pre_edit_policy.sh \
  "$(jq -n --arg command $'*** Begin Patch\n*** Update File: services/initd/src/main.rs\n@@\n-a\n+b\n*** End Patch' '{tool_input:{command:$command}}')"

expect_deny \
  "prompt token is denied" \
  .codex/hooks/user_prompt_policy.sh \
  "$(jq -n --arg prompt 'token sk-testtesttesttesttesttest1234567890' '{prompt:$prompt}')"

expect_match \
  "root policy routes physical GPU work" \
  AGENTS.md \
  'docs/ai/physical-gpu-status\.md'

expect_match \
  "root policy routes continued sessions" \
  AGENTS.md \
  'docs/ai/session-handoff\.md'

expect_match \
  "task router classifies physical GPU work" \
  docs/ai/task-router.md \
  'Physical GPU/VFIO continuation'

expect_match \
  "task router classifies session continuation" \
  docs/ai/task-router.md \
  'Resume prior work / prepare session handoff'

expect_match \
  "AI map links the handoff without extending the stable prefix" \
  docs/ai-map.md \
  'session-handoff\.md.*volatile checkout state'

expect_match \
  "session handoff preserves dirty work" \
  docs/ai/session-handoff.md \
  'worktree is intentionally dirty'

expect_match \
  "session hook routes live checkout state" \
  .codex/hooks/session_context.sh \
  'docs/ai/session-handoff\.md'

expect_match \
  "physical status preserves deferred FPS gate" \
  docs/ai/physical-gpu-status.md \
  'user-deferred'

expect_match \
  "physical status names the generic wait ABI" \
  docs/ai/physical-gpu-status.md \
  'atomic check-arm-recheck'

expect_match \
  "KVM skill distinguishes the userspace ABI" \
  .agents/skills/rustos-kvm/SKILL.md \
  'cross-service'

expect_match \
  "ripgrep MCP version is pinned" \
  .codex/config.toml \
  'mcp-ripgrep@0\.4\.0'

expect_match \
  "Serena MCP version is pinned" \
  .codex/config.toml \
  'serena-agent==1\.6\.0'

expect_match \
  "ast-grep MCP is configured" \
  .codex/config.toml \
  'mcp_servers\.ast_grep'

expect_match \
  "CodeGraph MCP is configured" \
  .codex/config.toml \
  'mcp_servers\.codegraph'

expect_match \
  "source edits require the three MCP tools" \
  AGENTS.md \
  'preflight all three project MCP servers'

expect_match \
  "RustOS code editing skill exists" \
  .agents/skills/rustos-code-editing/SKILL.md \
  '^name: rustos-code-editing'

expect_match \
  "RustOS code editing skill makes Serena primary" \
  .agents/skills/rustos-code-editing/SKILL.md \
  'Serena is the primary editor'

expect_match \
  "unified shell tool is hook-covered" \
  .codex/config.toml \
  'Bash\|exec_command\|mcp__serena__execute_shell_command'

expect_match \
  "Serena excludes generated output" \
  .serena/project.yml \
  'build/\*\*'

expect_match \
  "Serena points resumed sessions to the handoff" \
  .serena/project.yml \
  'docs/ai/session-handoff\.md'

expect_match \
  "handoff skill has a concrete trigger" \
  .agents/skills/rustos-session-handoff/SKILL.md \
  '^description: Resume or prepare a RustOS development session'

expect_no_match \
  "handoff skill contains no template TODO" \
  .agents/skills/rustos-session-handoff/SKILL.md \
  'TODO'

test -x tools/check-dev-environment.sh || {
  printf 'not ok - development environment checker is not executable\n' >&2
  exit 1
}
pass "development environment checker is executable"

for workflow in .github/workflows/*.yml; do
  expect_no_match \
    "$(basename "$workflow") actions are commit-pinned" \
    "$workflow" \
    'uses:[[:space:]]*actions/[^@[:space:]]+@v[0-9]'
  expect_no_match \
    "$(basename "$workflow") runner image is fixed" \
    "$workflow" \
    'runs-on:[[:space:]]*ubuntu-latest'
done

bash formal/selftest.sh >/dev/null || {
  printf 'not ok - formal model registry selftest failed\n' >&2
  exit 1
}
pass "formal model registry selftest passes"

for agent in .codex/agents/*.toml; do
  expect_match "$(basename "$agent") uses repository model" "$agent" \
    '^model = "gpt-5\.6-terra"$'
  expect_match "$(basename "$agent") uses repository reasoning" "$agent" \
    '^model_reasoning_effort = "xhigh"$'
done

printf 'RustOS Codex hook selftest passed\n'
