#!/usr/bin/env bash
# Codex PreToolUse hook for Bash tool when the command is a git commit.
#
# Runs `cargo fmt --check` and `cargo clippy` before the commit lands so
# the agent doesn't push style/lint regressions. Bypassable only by the
# user explicitly invoking with --no-verify (which pre_bash_destructive
# will already block — by design).

set -euo pipefail

INPUT="$(cat)"

cmd="$(printf '%s' "$INPUT" | jq -r '
  .tool_input.command // .arguments.command // .params.command // empty
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

# clippy is already run by post_edit_rust.sh after every .rs edit;
# re-running --workspace here adds ~120s per commit for no benefit.
exit 0
