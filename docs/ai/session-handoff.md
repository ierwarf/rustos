# Session Handoff

**Role:** volatile routing note for resuming this checkout. It is not build,
runtime, formal, or hardware evidence. Source, the live goal tracker, and fresh
command output win when they disagree with this page. This page is pruned
aggressively on purpose: a closed item's debugging narrative belongs in git
history, not here. Durable architecture lives in `structural-ownership-design.md`
and `performance-hardening.md`; the measurement record lives in
`docs/benchmarks/README.md`. Do not duplicate any of them here.

## Current checkout snapshot

Recorded 2026-08-31. **This block is the only current-state section.**

**Phase 7 (anonymous demand paging) is cut over and running.**
`PAGER_DEMAND_ADMISSION_WIRED` is `true` in `mm_broker_ops.rs`: anonymous
`mmap` publishes a kernel-stamped pager VMA, admits it into pagerd, and first
touch resolves through the ring0 fault path. A 1-vCPU boot resolves roughly
900 demand faults across 106 backing admissions with no panic, lockdep,
corruption, or `RunnableButUnqueued`.

### The defect that had capped this at 64 faults

pagerd's `consume()` appended every resolved token to a fixed
`consumed_tokens` list and never reclaimed an entry. Once 64 entries filled,
`get_mut(consumed_len)` returned `None`, so **every subsequent fault failed
with `Pressure`** and its task never resumed. Earlier sessions read the same
"exactly 64 completions then silence" as frame exhaustion, an allocator
lock-holder delay, and a userspace startup-ordering stop; it was none of
those. Symptom at the surface was ld.so reporting
`libc.so.6: cannot map zero-fill pages`.

The fix replaces that list with exact per-slot replay state. Fault tokens are
`(generation << PAGER_FAULT_TOKEN_SLOT_BITS) | slot` with a strictly
increasing per-slot generation, so pagerd keeps
`accepted_generations[PAGER_MAX_FAULT_SLOTS]` and rejects any token whose
generation is not newer than the one recorded for its slot. That is exact
one-shot semantics in fixed memory, with no ceiling on total faults. The token
shape was promoted out of `kernel/ps/src/multitask/pager_fault.rs` into
`libs/rustos-user-abi/src/pager.rs` (`pager_fault_token_slot`,
`pager_fault_token_generation`) so ring0 and pagerd read one definition rather
than pagerd reverse-engineering a kernel-private encoding.

### Known limitation: 8 vCPU is intermittent

1, 2, and 4 vCPU boots pass repeatedly. 8 vCPU is **marginal**: at `--timeout
30` it failed 3 of 4 runs, and at `--timeout 90` it passed 1 of 2, always with
the same single missing marker `smp-cpu-first-user-dispatch arg0=0x1` - CPU 1
does not receive user work in time. No panic, lockdep, corruption, or stale
marker appears in any failing run, and every service reaches readiness. The
likely owner is early user work serialising through the single pagerd fault
endpoint and its housekeeping dispatch turn, which is the throughput question
the pager phase was always going to raise. **Do not treat 8-vCPU as a passing
product gate.** Measure it before widening the envelope, and prefer a
structural answer (fault batching, or a second dispatch turn per housekeeping
pass) over raising timeouts.

### Fresh evidence for this change set

`cargo xtask check`, `cargo xtask build`, and `cargo xtask verify-dvm` pass.
`bash formal/verify-all.sh --profile pr` **sealed**: 44 artifacts, every lane
green, including implementation-mutations at 612 mutants with none surviving.
`formal/check-rust-source-contracts.py` passes at 489 Rust files and 107
critical/high surfaces. Unit tests: kernel-compat 160, kernel-ps 262,
kernel-mm 41, kernel-hal 66, kernel-executive 6, kernel-ipc-runtime 61,
pagerd 18, rustos-user-abi 42. KVM smoke passes at 1, 2, and 4 vCPU; 8 vCPU as
qualified above. `cargo fmt --all --check` is clean except three files with
pre-existing committed drift (nucleus lockdep preemption, two xtask files).

### Contract and registry work landed with the behaviour

- `formal/trust-boundaries.tsv`: the `pager-fault-policy` identity evidence is
  `request_sender_is_authorized`, which asserts the ring0 `(0, 0)` receive-side
  identity for `FAULT_RESOLVE` and keeps exact nonzero subject authentication
  for every user-originated op. Reaching pagerd's endpoint already requires
  `IPC_SERVICE_CAP_ROOT_SUPERVISOR`.
- Ordering and unsafe debt for the new pager code is documented rather than
  registered: `pager_fault.rs` 14 -> 0, `frame_capability.rs` 3 -> 0, and the
  new `irq.rs` unsafe block carries its `SAFETY:` note.
- `formal/rust-large-files.tsv` records the growth this change set caused and
  adds split plans for `ipc/tests.rs` and `current.rs`.
- `pager-fault-slot-claim-before-block` survived because its witness tested
  `claim_reply` while the mutant targets `take_next_dispatchable`. The new
  `dispatch_never_takes_a_slot_before_its_task_has_blocked` kills it, and the
  registry now names it. `pager-vma-publication-identity-bypass` was pinned to
  occurrence `1/2` (the `lookup` site its witness actually exercises).

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
- **Intermittent uiserver startup reply timeout.** Two restored-control bench
  boots in this session missed the 30-second readiness gate; the first relevant
  guest event was `ipc-service-reply-timeout` during Wayland initialization,
  followed by `uiserver: exiting with nonzero status errno=5`. Two candidate
  runs, the diagnostic run, and the 4-vCPU run then passed without a source
  change to that path, so this is not attributed to the lock-budget change. If
  it recurs, preserve the first timeout envelope and trace its exact service
  operation before proposing a fix.

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
