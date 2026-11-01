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
2. Find the owner contracts and dependent code via [task router](task-router.md).
   `AGENTS.md` is the stable instruction prefix, not this file.
3. Re-run focused native/model/contract checks for the exact modified
   slice. An old signed-image seal or benchmark cannot validate changed bytes.
4. Use `cargo xtask check` and the applicable `formal/verify-all.sh`
   profile before bounded KVM evidence. Do not report a source/model pass
   as live crash, hardware or performance verification.

## Implementation snapshot

- Ring3 eight-stage migration: the per-stage wired / model-only / missing table,
  fixed defects and open risks are in
  [Ring3 eight-stage status](../validation/ring3-8stage-status.md); the provider
  billing acceptance matrix and witness map are in
  [provider custody acceptance](../validation/ring3-provider-custody-acceptance.md).
- [Job model](../architecture/job-model.md): process/thread/page limits, one
  kmem class (ProcessObject bytes), client-funded sessions (runtimed, netd,
  vfsd) and JobPolicy ceilings are in source. Total kmem charging and full
  legacy authority migration remain open.
- [TLB redesign](../architecture/tlb-shootdown-design.md) is separate.
  Existing exact-ACK/reclaim safety must be preserved.
- [Commercial gates](commercial-quality-gates.md) and
  `formal/COVERAGE.md` determine what runtime/hardware evidence is missing.
- A formal seal covers the whole tree (including docs): any edit unseals it, and
  multi-vCPU `kvm-smoke` re-runs `verify-all.sh --profile pr` first. A 1-vCPU
  smoke (`--rustos-vcpus 1`) does not need the seal.

## Preserved dated evidence

[Handoff chronology](session-handoff-log.md) preserves detailed 2026-10-29/30
experiments, successful and failed boots, WIP policy3a checks and evidence
paths. They describe their original source/image trees, **not** a future
checkout or this docs-only change. Historical 40/40 boots and opt-in quota
attempts are not new results or a blanket stability claim.
