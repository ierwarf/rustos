# Session Handoff — repository snapshot, 2026-10-31

This is **not** a claim about an agent's current worktree. The GitHub
`master` head observed for this documentation pass was
`1d4c97b0aacb911a00e1d00a3983fbc6a1147a29` (2026-10-30 UTC).
A different local checkout can have newer commits, uncommitted edits or
a selected opt-in image; check these before executing or editing.

## Resume safely

1. Read `git status --short`, `git log -1`, the active goal and selected
   image/config. Preserve uncommitted work. Never infer a clean tree from
   this committed snapshot.
2. Find the smallest source/owner contract via [task router](task-router.md).
   `AGENTS.md` is the stable instruction prefix, not this file.
3. Re-run focused native/model/contract checks for the exact modified
   slice. An old signed-image seal or benchmark cannot validate changed bytes.
4. Use `cargo xtask check` and the applicable `formal/verify-all.sh`
   profile before bounded KVM evidence. Do not report a source/model pass
   as live crash, hardware or performance verification.

## Implementation snapshot, not acceptance

- [Job model](../architecture/job-model.md): phase 1, 2a, counted
  process/thread and resident user-page limits (2b), application Job
  placement and JobPolicy v1 ceilings are in source. Kernel-memory
  charging, `memory.high/min` and full legacy authority migration remain open.
- [Ring3 roadmap](../architecture/ring3-process-model.md): IDL/runtime,
  component supervision, client-funded sessions, personality servers and
  operability are later stages. Partial 3a policy support is not a completed
  capability cutover.
- [TLB redesign](../architecture/tlb-shootdown-design.md) is separate.
  Existing exact-ACK/reclaim safety must be preserved.
- [Commercial gates](commercial-quality-gates.md) and
  `formal/COVERAGE.md` determine what runtime/hardware evidence is missing.

## Preserved dated evidence

[Handoff chronology](session-handoff-log.md) preserves detailed 2026-10-29/30
experiments, successful and failed boots, WIP policy3a checks and evidence
paths. They describe their original source/image trees, **not** a future
checkout or this docs-only change. Historical 40/40 boots and opt-in quota
attempts are not new results or a blanket stability claim.

No compilation, KVM execution, benchmark or formal re-verification is
claimed by this Markdown-only update.
