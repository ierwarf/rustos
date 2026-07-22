# Session Handoff

**Role:** volatile routing note for resuming this checkout. It is not build,
runtime, formal, or hardware evidence. Source, the live goal tracker, and fresh
command output win when they disagree with this page.

## Current checkout snapshot

Recorded on 2026-07-22 before a session switch:

- The goal tracker reports no active goal. Do not silently recreate an older
  goal; register a new one only when the user explicitly asks for it.
- The worktree is intentionally dirty across formal models, kernel and service
  hardening, AI infrastructure, and supporting tools. Preserve all existing
  tracked and untracked work. Never use `reset`, `clean`, or broad `restore` to
  make the checkout look tidy.
- `formal/COVERAGE.md` is the acceptance ledger. Re-run the gate relevant to a
  new claim; do not turn a recorded model result into current source or runtime
  evidence.
- Physical GPU state, evidence limits, and the generic userspace wait-set's
  remaining release gates live only in `physical-gpu-status.md`. Do not start,
  bind, reset, or retry
  hardware merely because a new session began.
- Documentation, skill, hook, Serena, formal-model, and RustOS-only changes do
  not require a Linux DVM rebuild. Route any real DVM change through the
  `rustos-build` and `rustos-kvm` skills and their cached-build rules.

## Resume sequence

1. Read the stable prefix: `AGENTS.md`, `docs/ai-map.md`, `token-policy.md`, and
   `task-router.md`.
2. Query the live goal state, then run `git status --short` and a focused
   `git diff --stat`. Treat both as inspection only; do not normalize the
   checkout.
3. Route the new user request through `task-router.md`. Read this page again
   only for continuation or handoff work, not as a universal fifth prefix.
4. Use Serena or ripgrep for scoped discovery. If either MCP server is absent
   or fails, continue with local `rg`; MCP availability is not a product gate.
5. After edits, run `cargo xtask dev-plan` and execute only the relevant lanes.
   For AI-infrastructure changes, also run `.codex/hooks/selftest.sh` and
   `tools/check-dev-environment.sh --ai`.

## Refresh rule

Update this page only when preparing another handoff or when the live goal,
major blocker, hardware safety boundary, or validation ownership changes.
Keep durable architecture in the focused AI contracts and detailed pass/fail
evidence in its owning ledger; do not duplicate either here.
