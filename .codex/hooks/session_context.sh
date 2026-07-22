#!/usr/bin/env bash
# Codex SessionStart hook.
# Adds compact RustOS-specific guardrails without requiring agents to reread
# broad docs before every focused task.

set -euo pipefail

jq -n '{
  hookSpecificOutput: {
    hookEventName: "SessionStart",
    additionalContext: "RustOS hook context: read the AGENTS.md stable prefix and route through docs/ai/task-router.md. For continued work, read docs/ai/session-handoff.md, query live goal state, and audit git status before editing; preserve the dirty worktree. Prefer Serena/ripgrep but fall back to local rg when MCP is unavailable. For physical GPU work read docs/ai/physical-gpu-status.md before source or hardware. Never treat visual or model output as physical performance evidence. After edits use cargo xtask dev-plan; run the lanes it selects."
  }
}'
