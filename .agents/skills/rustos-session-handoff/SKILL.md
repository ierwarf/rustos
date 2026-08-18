---
name: rustos-session-handoff
description: Resume or prepare a RustOS development session without losing goal, dirty-worktree, evidence, or hardware-safety state. Use when the user asks to continue prior RustOS work, switch to a new session, recover context, or produce a handoff. Skip isolated fresh tasks that do not depend on checkout history.
---

# RustOS Session Handoff

## Resume

1. Read the root `AGENTS.md` stable prefix, then
   `docs/ai/session-handoff.md` — **its "Current checkout snapshot" section
   only**. Everything from "Session log" onward is a dated archive whose early
   entries are superseded in place further down; reading it top-down is how a
   stale claim gets treated as current. Do not load unrelated AI contracts.
2. Query the live goal state. Never infer that a documented old objective is
   active, and create a goal only when the user explicitly requests one.
3. Run `git status --short` and a focused `git diff --stat`. Preserve every
   existing tracked and untracked change unless ownership is proven.
4. Route the new request through `docs/ai/task-router.md`, then inspect only
   its owning source or evidence ledger.
5. Classify prior results as recorded evidence, not current proof. Re-run only
   the gate needed for the new claim.

## Tool and runtime boundaries

- Prefer Serena or ripgrep MCP for scoped discovery. Fall back to local `rg`
  when MCP is unavailable; do not block product work on an indexing service.
- Do not launch KVM, rebuild the Linux DVM, or mutate physical hardware simply
  to restore context. Apply `rustos-build` or `rustos-kvm` only when the current
  user request actually enters those scopes.
- A dirty worktree is not a repair task. Never normalize it with destructive
  Git commands.

## Prepare a handoff

When the user asks to switch sessions:

1. Update `docs/ai/session-handoff.md` only for live goal state, major blockers,
   hardware safety boundaries, and evidence ownership.
2. Keep durable design in the focused contract and detailed results in the
   owning ledger. Do not duplicate long diffs, logs, or pass inventories.
3. Run `.codex/hooks/selftest.sh`, `tools/check-dev-environment.sh --ai`, and
   `git diff --check`. Run `cargo xtask dev-plan` after edits; its output routes
   validation but is not evidence.
4. Report the changed handoff pointers, validation, and any remaining blocker
   concisely.
