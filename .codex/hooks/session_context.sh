#!/usr/bin/env bash
# Codex SessionStart hook. Keep injected context tiny; detailed policy is on demand.

set -euo pipefail

jq -n '{
  hookSpecificOutput: {
    hookEventName: "SessionStart",
    additionalContext: "RustOS: AGENTS.md is the only stable repo prefix; other docs are on demand. Resume via docs/ai/session-handoff.md + git status. Keep reads/output bounded."
  }
}'
