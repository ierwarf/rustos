#!/usr/bin/env bash
# Codex SessionStart hook. Keep injected context tiny; detailed policy is on demand.

set -euo pipefail

jq -n '{
  hookSpecificOutput: {
    hookEventName: "SessionStart",
    additionalContext: "RustOS: stable prefix is AGENTS.md + docs/ai/task-router.md. Resume via docs/ai/session-handoff.md + git status. Serena is primary; ast-grep/CodeGraph are conditional and unused MCPs are not preflighted. GPU continuation reads docs/ai/physical-gpu-status.md. Keep reads/output bounded."
  }
}'
