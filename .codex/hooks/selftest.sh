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

expect_deny \
  "destructive rm is denied" \
  .codex/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg command 'rm -rf hook_probe_nonexistent' '{tool_input:{command:$command}}')"

expect_quiet_allow \
  "read-only search for dangerous text is allowed" \
  .codex/hooks/pre_bash_destructive.sh \
  "$(jq -n --arg command 'rg -n "rm -rf" .codex/hooks' '{tool_input:{command:$command}}')"

expect_deny \
  "Cargo.lock read is denied" \
  .codex/hooks/pre_read_large_file.sh \
  "$(jq -n '{tool_input:{relative_path:"Cargo.lock"}}')"

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
  "task router classifies physical GPU work" \
  docs/ai/task-router.md \
  'Physical GPU/VFIO continuation'

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

for agent in .codex/agents/*.toml; do
  expect_match "$(basename "$agent") uses repository model" "$agent" \
    '^model = "gpt-5\.6-terra"$'
  expect_match "$(basename "$agent") uses repository reasoning" "$agent" \
    '^model_reasoning_effort = "xhigh"$'
done

printf 'RustOS Codex hook selftest passed\n'
