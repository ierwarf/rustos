#!/usr/bin/env bash
# Codex SessionStart hook.
# Adds compact RustOS-specific guardrails without requiring agents to reread
# broad docs before every focused task.

set -euo pipefail

jq -n '{
  hookSpecificOutput: {
    hookEventName: "SessionStart",
    additionalContext: "RustOS hook context: route through docs/ai/token-policy.md and docs/ai/task-router.md; do not read logs/target/build/vendor/Cargo.lock wholesale; preserve Linux ELF and Windows PE compatibility; keep kernel policy evacuation service-owned; run repo hooks and prefer cargo xtask check for broad validation."
  }
}'
