#!/usr/bin/env bash
# Codex PreToolUse hook for Bash tool when the command is a git commit.
#
# Runs formatting and repository-policy checks before the commit lands so
# the agent doesn't push structural regressions. Bypassable only by the
# user explicitly invoking with --no-verify (which pre_bash_destructive
# will already block — by design).

set -euo pipefail

INPUT="$(cat)"

cmd="$(printf '%s' "$INPUT" | jq -r '
  .tool_input.command // .tool_input.cmd //
  .arguments.command // .arguments.cmd //
  .params.command // .params.cmd // empty
' 2>/dev/null || true)"

if [[ ! "$cmd" =~ git[[:space:]]+commit ]]; then
  exit 0
fi

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT" 2>/dev/null || {
  exit 0
}

fail() {
  jq -n --arg m "$1" '{
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

if ! cargo fmt --all -- --check >/dev/null 2>&1; then
  fail "cargo fmt --check failed. Run 'cargo fmt --all' and retry."
fi

if ! .codex/hooks/selftest.sh >/dev/null 2>&1; then
  fail "RustOS agent/hook policy selftest failed. Run '.codex/hooks/selftest.sh' and repair the reported contract drift."
fi

# `cargo xtask check` is run by the post-edit hook for changed Rust content.
# Generic workspace clippy is not claimed here: it lacks xtask's generated
# build configuration and is not currently an admitted repository gate.
exit 0
