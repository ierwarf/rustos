#!/usr/bin/env bash
# Codex SessionStart hook. Keep injected context short; the stable prefix owns
# detailed policy and should remain cacheable.

set -euo pipefail

jq -n '{
  hookSpecificOutput: {
    hookEventName: "SessionStart",
    additionalContext: "RustOS: use the stable prefix + docs/ai/task-router.md. Resume via docs/ai/session-handoff.md and git status. Source edits require Serena+ast-grep+CodeGraph; GPU work also reads docs/ai/physical-gpu-status.md. After edits run xtask dev-plan and selected lanes. Keep reads/logs bounded."
  }
}'
