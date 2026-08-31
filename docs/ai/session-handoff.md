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

### pagerd region tracking was leaking, and the downgrade was silent

`invalidate_process` was only ever called from a unit test: **nothing in
production released a pagerd region**. Ring0 does free its own VMA slot in
`broker_unmap`, so release existed on one side of the contract only. Once the
fixed table filled, `admit_region` returned `Pressure` forever, and
`broker_map_anon` fell back to eager mapping **without any signal** - demand
paging quietly stopped being used. A dead region also refused re-admission of
its own range as an overlap, so re-mapping a freed range could never recover.

The lifecycle is now symmetric. `unmap_for_process` returns the stamped
`(process_handle, process_generation)` it released, so the caller names exactly
what ring0 freed instead of re-deriving an identity that could disagree; the
broker forwards that as `COMMERCIAL_MAX_PAGERD_OP_RELEASE_OBJECT`, and pagerd
drops the matching regions. Capacities moved into the shared ABI with their
relation written down (`PAGER_MAX_VMAS_PER_PROCESS`,
`PAGER_MAX_TRACKED_REGIONS = 4 x` that), replacing three independent `64`s that
had no declared relationship. Admission refusal now emits
`pager-backing-admission-refused`, so the eager downgrade can never be silent
again. Pager dispatch also pipelines up to 8 faults per housekeeping turn
instead of one.

Measured on a 1-vCPU boot: admission refusals **0**, and
`pager-anon-fault-progress` rose from 14 to 25 milestones (~900 -> ~1600 faults
actually served by the pager).

### Closed: 8-vCPU CPU1 user-dispatch starvation

The failure was scheduler placement, not pager throughput or missing CPU/IPI
readiness. `runqueue_target_cpu` computed the exact last-CPU/slot-spread target
and then unconditionally replaced it with the globally least-loaded queue. One
long-lived kernel continuation on CPU1 therefore erased residue 1 from user
placement even when the difference was only one queued task; CPU1 kept running
kernel work but never received a user continuation.

Placement now preserves the exact locality/spread target while its queued
count is at most one above the least-loaded eligible CPU, matching the existing
active-balance threshold. It overrides the target only for a greater-than-one
imbalance. A focused regression fixes the 0/1/2 and 3/4/5 boundaries.

Fresh exact-tree evidence: the focused kernel-ps test, both scheduler TLC
models, `cargo xtask check`, `cargo xtask build`, and the PR formal seal pass.
Across 15 freshly built 8-vCPU boots, `smp-cpu-first-user-dispatch arg0=0x1`
was never missing. The control is just as sharp: on the same commit with the
placement override restored, it was missing in 5 of 5 runs at `--timeout 60`
and 3 of 3 at `--timeout 120`, so the CPU never recovered given four times the
budget. `kvm-smoke` also passes at 1, 2, and 4 vCPU.

**Method warning, and it invalidated a whole session of conclusions.**
`cargo xtask kvm-smoke` does **not** rebuild the image; it boots whatever is
already in `build/image/`. Every earlier "instrumentation produced no output
from `kernel-executive` or `kernel-compat`" note came from probes that were
never compiled into the booted kernel, and the hypotheses that session recorded
as *rejected* were tested against a stale image. Treat them as untested, not
disproved. Always `cargo xtask build` after a source change, then refresh
`formal/verify-all.sh --profile pr` (~25 s; the lanes cache) before drawing any
multi-vCPU conclusion. Multi-vCPU runs check the seal against the current
source hash, and a stale seal fails with `formal verification run binding
mismatch`, which reads exactly like a boot failure.
`--smp-iteration` needs its own `formal/verify-smp-iteration.sh` seal.

### Closed: the AP trampoline range was never reserved

`boot.rs` claimed physical `0x8000..0xA000` - the AP trampoline and startup
mailbox - only under `if hal_api::cpu::discovered_count() > 1`. But
`cpu_count()` returns **0 until the topology registry is published**, and that
publication happens later, in `init_acpi`. The guard was therefore false on
every boot at every vCPU count, and the claim never ran.

`ap_trampoline::install` then wrote the trampoline into memory the allocator
still listed as free, and `ap_trampoline::seal` marked both pages read-only in
the direct map. Whichever allocation next received one of those two frames
panicked the kernel on its first write:
`Unhandled exception: vector = 14, error code = Some(3)` inside `memcpy`, with
`cr2` always inside `0xffff_8000_0000_8000..0xA000`. It was intermittent only
because it needed the allocator to hand out exactly those two frames, which
the demand-paging workload makes likely and a quiet boot does not.

The claim is now unconditional and runs immediately after `init_phys`, before
anything can allocate. `OutsideUsableMemory` is tolerated (firmware already
withheld the range); `AlreadyOwned` fails loudly.

**The contract fix matters more than the one-line fix.** Two facts had to agree
- "this range is reserved" and "this range is read-only" - and they lived in
different crates with nothing linking them. The seal now asserts
`phys::range_is_withheld_from_allocation` before removing write permission, so
the two cannot drift apart again whatever the cause. `discovered_count()` also
documents that `0` means *not known yet*: every other caller already defended
against it with `assert!((1..=MAX).contains(&count))`, and the trampoline claim
was the only place that used it as a bare predicate.

Evidence: plain `kvm-smoke --rustos-vcpus 8` went from 4 of 6 passing with
kernel panics to **6 of 6 with zero panics**, twice over; 1, 2, and 4 vCPU pass.
The `cr2 = 0x8xxx/0x9xxx` panic class does not appear in any run since.

### Closed: runtimed logged the machine to a standstill

`load_launch_catalog_into_state` emitted its `launch catalog load begin` pair on
every broker pass while an off-loop catalog load was still in flight, rather
than once per dispatched load. A slow storage read produced **2,204 of them in
one 30 s window - 4,408 debugcon lines**. Each debugcon line is a synchronous
port write taken under a global lock with interrupts disabled, so the
diagnostic became the dominant machine-wide stall and extended the very read it
was reporting on. That is the 30 s gap between uiserver's `init open_display
done` and `init open_input begin`.

The begin line now marks one dispatched load. Measured: the same gap is **6 ms**,
the boot log dropped from thousands of lines to ~350, and a healthy boot logs
3-4 begins.

### Contention panics now name the code that has to change

`ProcessStateLock` contention panics (`ProcessStateLock contention cannot
block`) reported `process_table/identity.rs` as both the owner and the waiter -
the one file that is never at fault, because it holds nothing but the thin
`with_*` accessors. That named neither the subsystem that took the lock nor the
one that could not wait for it, which is the only thing the panic exists to
say. The accessors now capture their own `#[track_caller]` location and hand it
to `ProcessStateLock::lock_at`, so the attribution survives the accessor layer.

The panic itself is still open, and it is worth stating plainly what its
contract costs: a lock taken from a context that can neither block nor tolerate
100,000 spins turns transient SMP contention into a dead machine.
`process_table::try_with_process_state_mut` already exists precisely because
the exception path must fail closed instead, so the shape of the fix is known -
what is missing is knowing which caller is in the non-blockable context, which
is what the attribution above now supplies. Reproduce under
`--min-ui-fps 60 --rustos-vcpus 8`; it appeared in roughly one run in four.

### Open: the `--min-ui-fps 60` gate does not pass, at any vCPU count

This is **not** an SMP problem: `--min-ui-fps 60` fails identically at 1 vCPU
and at 8, so do not spend another session treating it as one. The CPU1
placement fix above is unrelated to it and is not blocked on it.

The gate now implies `--gui-dvm-surfaces` (a `kvm/options.rs` change kept in
this change set: a GTK consumer without the shared display aperture can only
render QEMU's unrelated guest console, so the old gate proved the wrong thing).
That makes it strictly harder, and it currently stops in two places:

The gate now reaches the FPS proof and WayClick sustains **66-116 FPS across
16 one-second windows**, so the frame rate itself is not the problem. Two
things still stop it:

- **One ~817 ms frame, every run.** `uiserver: slow gpu submit` charges it
  entirely to `rebuild_scene_us`, `desktop refresh elapsed_ms=817` charges it
  entirely to `refresh_desktop_surface`, and the phase split charges **all of
  it to the chrome-strip restore**: `strips_us=816868 rails_us=377
  launchers_us=18`. The actual drawing costs 0.4 ms. The strip restore is a
  ~768 KB row copy, and the debugcon log shows a dense contiguous burst of
  `pager-anon-fault-progress` immediately before it. So this is **anonymous
  demand paging: one pagerd IPC round trip per 4 KiB of first touch**,
  landing synchronously on the UI thread mid-frame. It wrecks exactly one
  window (33 FPS) and trips the harness's slow-loop rule. The obvious fix is
  fault-around - resolve a bounded run of adjacent pages per fault instead of
  exactly one - which is a pager protocol change and wants its own change set.
  Do not look for a rendering bug; the renderer is innocent.
- **`fixed input-ring credit timeout outstanding=1279 limit=1279`**, and some
  runs where the Linux DVM guest exits before readiness. Not yet root-caused.

One measurement worth keeping: timed waits overshoot badly under the input
exercise. `runtimed: idle wait overshoot budget_us=10000 elapsed_us=~30000` is
a consistent 3x, and `inputd` charges `log_us=~1400` per turn - debugcon port
writes, taken under a global lock with interrupts disabled, dwarf that turn's
`drain_us`/`decode_us`. A 60 fps proof needs 16.6 ms frames, so the wake
latency and the logging cost are both plausible first suspects.

Two in-flight changes belong to this gate and are kept because they are
independently justified: `INPUT_INGESTION_WATCHDOG_MS` 100 -> 25 ms (verified
load-bearing - with the committed 100 ms value the run dies on
`fixed input-ring credit timeout outstanding=1279 limit=1279 timeout_ms=50`,
because the consumer must repoll and publish credit before L0's 50 ms credit
watchdog fails closed) and the `--min-ui-fps` topology change above.

### Log volume and CI

A 30-second 1-vCPU boot logged 839 lines, of which 212 were a
begin/complete pair per pager admission and about a hundred were scheduler
census rows whose count and denominator were both zero. Per-admission logging
is gone (refusals and the rate-limited `pager-anon-fault-progress` carry the
signal) and `record_census_row` drops only information-free rows - a zero count
against a nonzero denominator is a real observation and is kept. Boot logs are
now 547 lines.

CI is deliberately small: formatting, `cargo xtask config check`,
`cargo xtask check`, and the host test set. The formal gate, QEMU/KVM runs, and
docs publishing are local commands, not CI jobs; `formal-nightly.yml` was
removed with them. CI previously ran `cargo test -p driver-abi`, which is not a
workspace package, so the host-test step could never have passed.

### Fresh evidence for this change set

`cargo xtask check`, `cargo xtask build`, and `cargo xtask verify-dvm` pass.
`bash formal/verify-all.sh --profile pr` **sealed**: 44 artifacts, every lane
green, including implementation-mutations at 612 mutants with none surviving.
`formal/check-rust-source-contracts.py` passes at 489 Rust files and 107
critical/high surfaces. Unit tests: kernel-ps 263, kernel-compat 160, xtask 147,
rustos-user-abi 42, driver-domain-protocol 21, runtime-control 15.
`cargo fmt --all -- --check` is clean.

Plain `kvm-smoke` passes at 1, 2, 4, and 8 vCPU. The 8-vCPU run is no longer
CPU1-starved in any of 15 boots. It is still intermittent for a *different*
reason - 2 of 6 repeats missed
`uiserver: gpu-scene compiler ready contract=3`, the same uiserver GPU
readiness latency the `--min-ui-fps` section above is open on. Do not read that
residual flake as the placement bug returning: check which marker is missing
before concluding anything.

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
