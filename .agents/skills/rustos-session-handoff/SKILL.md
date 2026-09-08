---
name: rustos-session-handoff
description: Resume or prepare a RustOS development session without losing live goal state, dirty-worktree ownership, runtime evidence, or hardware-safety boundaries. Use for continuation, recovery, session switching, or handoff requests.
---

# RustOS Session Handoff

## Resume

1. Use the already-loaded root `AGENTS.md`; do not preload the task router.
2. Read only the current checkout snapshot in
   `docs/ai/session-handoff.md`; its historical session log is not current
   evidence.
3. Query live goal state and run `git status --short` plus a focused diff stat.
4. Preserve all tracked and untracked changes. Never reset or clean a dirty
   worktree to make the handoff convenient.
5. Re-run only the gate needed for the new request; recorded results are not
   fresh proof.

For any source edit, load `rustos-code-editing` and select its smallest
Local/Structural/Critical tool tier; do not preflight unused MCPs.

## Prepare

Update `docs/ai/session-handoff.md` only with live goal state, major blockers,
hardware-safety limits, and evidence ownership. Put durable design in the
focused AI contract and detailed measurements in their owning ledger.

Run `.codex/hooks/selftest.sh`, `tools/check-dev-environment.sh --ai`,
`git diff --check`, and `cargo xtask dev-plan` after edits. The last command
routes validation but is not evidence. Report changed pointers, validation,
and blockers concisely.
