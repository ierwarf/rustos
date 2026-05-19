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
  printf '{"decision":"allow"}\n'
  exit 0
fi

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT" 2>/dev/null || {
  printf '{"decision":"allow"}\n'
  exit 0
}

fail() {
  jq -n --arg m "$1" '{decision:"block", message:$m}'
  exit 0
}

if ! cargo fmt --all -- --check >/dev/null 2>&1; then
  fail "cargo fmt --check failed. Run 'cargo fmt --all' and retry."
fi

if ! timeout 120 cargo clippy --workspace --no-deps -- -D warnings >/tmp/clippy.out 2>&1; then
  tail="$(tail -n 30 /tmp/clippy.out)"
  fail "cargo clippy failed (tail): $tail"
fi

printf '{"decision":"allow"}\n'
