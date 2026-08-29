# Session Handoff

**Role:** volatile routing note for resuming this checkout. It is not build,
runtime, formal, or hardware evidence. Source, the live goal tracker, and fresh
command output win when they disagree with this page. This page is pruned
aggressively on purpose: a closed item's debugging narrative belongs in git
history, not here. Durable architecture lives in `structural-ownership-design.md`
and `performance-hardening.md`; the measurement record lives in
`docs/benchmarks/README.md`. Do not duplicate any of them here.

## Current checkout snapshot

Recorded 2026-08-29. **This block is the only current-state section.** The
working tree is clean at `85c8a0b1` (pagerd extracted into its own service;
two verification gaps this exposed — the zero-trust ingress discovery regex
and a missing trust-boundaries.tsv row — fixed at the root rather than
special-cased). `formal/selftest.sh` passes. `cargo xtask kvm-smoke
--gui-dvm-surfaces --min-ui-fps 60` passed with sustained 60,000–66,920
`frame_hz_milli` across multiple windows, measured this session.

## Known open items

Verified against current source where noted; otherwise carried forward from
the last investigation without re-verification, and flagged as such.

- **`V5-SCHED-GLOBAL-001`.** Source-verified 2026-08-29: the former
  `TaskContext.ready` duplicate authority has zero production readers left
  (`grep -rn '\.ready\b' kernel/ps/src/multitask` returns nothing outside
  test/struct-definition noise); the owner word is now the sole runnability
  authority, closing the stage this file previously called "scoped but not
  started" — that note was stale. The per-CPU runqueue, owner-word state
  machine, and remote-wake mailboxes are lock-free and already do not scan a
  global ready set. What the global `SCHEDULER` `TrackedSpinLock` still
  protects is lifecycle/catalog bookkeeping (`retired`, `starts`,
  `scheduling_domains` budget/custody, exec quiescing) — not dispatch
  selection. Ordinary-path acquisition-zero (dispatch itself, the reply-wake
  handoff, pick hints, retired-task cleanup) is still open. Full detail and
  the measured cost breakdown: `structural-ownership-design.md` §2, which is
  current and does not need re-deriving. Do not treat this item's title
  ("remove the global lock") as the task; §2.1c has the measurement-backed
  case that the critical section, not the lock, is the target.
- **`V5-GPU-UI-OWNER-014` / `V5-WAYLAND-HOL-013`.** uiserver's main loop is
  still one sequential owner: input, then Wayland dispatch, then render, then
  present. Narrow fixes landed (frame-callback permit regranted on
  `PresentUpdateResult::Idle`, backpressure no longer withholds the permit),
  and this session's own `--min-ui-fps 60` run passed, so the risk this item
  names is mitigated in practice. The general protocol/scene/submission owner
  split is still open; prior measurement found it would not move frame rate,
  so do not schedule it as a performance fix — only as ownership hardening,
  with frame records as the before/after check.
- **`kernel/mm/src/memory/heap.rs` compiles the production allocator into host
  test binaries.** Verified still present 2026-08-29: `LockedHeap`,
  `HEAP_ORDER`, and the size constants are `#[cfg(not(test))]` rather than
  gated on `rustos_boot_image`, the predicate `debug/mod.rs` and
  `input/wait_queue.rs` already use for the same class of mistake. Has not
  crashed a test run, but it is the same defect shape as the debug-port
  `cfg(test)` bug that did (fixed). Deliberately deferred: fix it standalone,
  using `kernel-ps`/`kernel-compat` test suites as the check, not appended to
  an unrelated change.
- **Uiserver → WayClick "Malformed Wayland message" (`Protocol error 0`),
  intermittent.** Not re-verified this session. Last investigation: ruled out
  concurrent writers and a stream-position desync on error paths; found and
  fixed a real but likely-unrelated ABI defect (an oversized `sendmsg`
  answered `EINVAL` instead of taking a prefix). The symptom itself was never
  confirmed reproduced or root-caused — one 8-vCPU run sustained 113 WayClick
  windows with no failure. If it recurs, check the receive-side segment
  reassembly next, per the last session's notes, before re-deriving from
  scratch.

## Resume sequence

1. Read the stable prefix: `AGENTS.md`, `docs/ai-map.md`, `token-policy.md`,
   `task-router.md`.
2. Check live goal state, then `git status --short` and `git diff --stat`,
   as inspection only. If the worktree is intentionally dirty, preserve it —
   never reset, checkout, clean, or otherwise discard its changes.
3. Route the new request through `task-router.md`. Re-read this page only for
   continuation/handoff work, not as a universal prefix.
4. Use Serena or ripgrep for scoped discovery; fall back to local `rg` if an
   MCP server is absent, without treating that as a product gate.
5. After edits, run `cargo xtask dev-plan` and its selected lanes. For
   AI-infrastructure changes, also run `.agents/hooks/post_edit_rust.sh`'s
   checks and `tools/check-dev-environment.sh --ai`.

## Refresh rule

Update this page only when preparing another handoff or when the live goal,
major blocker, hardware safety boundary, or validation ownership changes.
When an item here closes, delete its entry rather than marking it closed —
git history is the record of how it was fixed. Keep durable architecture in
the focused AI contracts and detailed pass/fail evidence in its owning
ledger; do not duplicate either here.
