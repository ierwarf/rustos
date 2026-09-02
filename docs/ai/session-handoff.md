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

### Closed: the ring0/pagerd map divergence on a partial unmap

Ring0's VMA table preserved the left and right remainders of an interior
`munmap`; pagerd deleted **every overlapping region**. A later fault in a
surviving remainder passed ring0's VMA check, reached pagerd, matched no
region, and killed the thread. `munmap(2)` in the middle of a mapping leaves
two smaller mappings on either side, so ring0 was right and pagerd was wrong -
but nothing in either module could say so, because each derived its own
remainders.

There is now one definition: `libs/rustos-user-abi/src/pager/region_edit.rs`.
`apply_region_edit` returns the exact surviving fragments for the five possible
outcomes, with backing offsets shifted; both replicas call it and neither
reimplements it. Each side is tested against that rule
(`ring0_rewrite_matches_the_shared_range_edit_rule`,
`an_interior_release_keeps_both_remainders_and_they_still_fault`), so proving
each equals the rule proves the two agree.

Two related holes closed with it:

- **`mprotect` never reached pagerd.** `reply.frame_rights` comes from
  `region.prot`, so a narrowing ring0 applied kept granting the old rights.
  `COMMERCIAL_MAX_PAGERD_OP_PROTECT_OBJECT` publishes it; both edit kinds
  reconcile through the one parked-release queue, where `prot == 0` means
  release.
- **`PROT_NONE` is the one declared asymmetry.** Ring0 keeps a deny-all VMA so
  the address stays owned; pagerd keeps nothing, because a span with no rights
  raises no fault and is not a canonical wire region. `pager_fragments()` is
  where that is written down.

**The direction of disagreement is the load-bearing rule.** Under pressure a
replica keeps *more* than ring0, never less: a pagerd region outliving its VMA
is inert (ring0 gates every dispatch), while a missing one kills a thread. So a
split with no free slot keeps the whole region and refuses with
`PAGER_PRESSURE_REGION_SPLIT_NO_SLOT` for the broker's parked retry.

Full contract: `docs/ai/pager-protocol-contract.md`. Model:
`formal/pager-region-agreement/PagerRegionAgreement.tla`, whose invariant
`FaultableIsAlwaysBackedByThePager` kills a registered wholesale-removal mutant.

### Closed: the wired fault reserve had no admission point to refuse at

The reserve was 64 frames behind a 128-slot fault table, and only housekeeping
replenished it. Making the fault path a direct rendezvous cut the housekeeping
turns that had been quietly doing that work; the reserve drained, and the first
visible symptoms were a dead user thread, an absent `devmgrd` endpoint, and a
failed boot - **not** "pager reserve exhausted".

Two fixes, and the second matters more. Completion now replenishes before it
wakes the fault owner, closing the frame lifecycle on the path that consumes
it. And the reserve is now sized to the fault-slot table
(`PAGER_WIRED_FAULT_FRAMES == PAGER_MAX_FAULT_SLOTS`), so it can only run dry
*after* fault-slot admission has already refused - a counted refusal instead of
an exhaustion with nothing to report it. `pager_fault_reserve_low_watermark()`
is the check; a boot that reaches `0` has violated the progress condition
whatever else looks healthy.

The three independent `64`s are gone. Capacities live in the shared ABI with
their relations static-asserted where both sides are visible, and
`PagerFaultError::Pressure` now carries a `PAGER_PRESSURE_*` code so a full
region table, an empty reserve, an exhausted grant table and a full release
queue no longer read identically in the log.

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

**Fixed: `kvm-smoke` used to boot a stale image, and it invalidated a whole
session of conclusions.** The lane launched whatever was already in
`build/image/`. Every earlier "instrumentation produced no output from
`kernel-executive` or `kernel-compat`" note came from probes that were never
compiled into the booted kernel, and the hypotheses that session recorded as
*rejected* were tested against a stale image. **Treat every such note as
untested, not disproved.**

`cargo xtask kvm-smoke` now refreshes the boot image itself before it copies
the disk (`crate::build::build`, pinned by
`a_smoke_run_refreshes_the_boot_image_before_it_copies_it`). A no-op build is
about two seconds against a thirty-second boot, so the lane pays it every time.
`--no-build` opts out for the rare case of deliberately booting the artifact
already on disk.

Still refresh `formal/verify-all.sh --profile pr` by hand (~25 s; the lanes
cache) before drawing any multi-vCPU conclusion. Multi-vCPU runs check the seal
against the current source hash, and a stale seal fails with `formal
verification run binding mismatch`, which reads exactly like a boot failure.
`--smp-iteration` needs its own `formal/verify-smp-iteration.sh` seal.

**Measure a pass rate with one command.** An 8-vCPU defect that appears in one
boot of six is a rate, and a single run cannot measure it. `--repeat <count>`
boots the same topology up to 64 times, prints each run's outcome, and names
every failed run's panic line and archived debugcon log:

```
cargo xtask kvm-smoke --rustos-vcpus 8 --min-ui-fps 60 \
    --dvm-network-shmem --timeout 120 --repeat 6
```

Repeating the lane in a shell loop instead loses each failing run's debugcon
log to the next run's truncation - which is exactly the evidence a rare defect
leaves behind. `cargo xtask soak` remains the equivalent for the `bench` lane.

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

### Closed: the `--min-ui-fps 60` gate now passes, and how

The gate passes at 8 vCPU. Measured this session: **5 of 6** with
`--min-ui-fps 60 --dvm-network-shmem --timeout 120`, zero panics in those runs.

The fix was to stop routing the page-fault path through a third,
fairness-scheduled task. A fault used to be queued by exception ingress and
then *forwarded* by `nucleus_housekeeping_task`, and its reply *adopted* by the
same task on a later turn - two scheduler round trips per 4 KiB, on a task with
no priority, which is why one 768 KiB row copy cost 817 ms.

The pager fault is now a fixed-slot rendezvous, `scheduler/pager_handoff.rs`
plus `SYS_RUSTOS_PAGER_FAULT_WAIT`/`_REPLY`. Exception ingress commits the
faulting task's `BlockReason::PagerFault(token)` wait and hands off directly to
a pagerd worker parked in `BlockReason::PagerService`; pagerd replies through
its own syscall, which maps the page and wakes the faulter. Housekeeping is out
of the path in both directions. Nothing on that path allocates, consults a
generic endpoint or reply object, or touches the process registry - which is
exactly what made the earlier attempt impossible, because the generic enqueue
path stores its request in a `Vec<u8>`.

This matches the reference designs rather than inventing one. QNX parks the
faulting thread in `WAITPAGE` while the memory manager services it; seL4 and L4
deliver faults as IPC to a fault-handler endpoint; all three switch directly to
the pager instead of making it runnable and re-running the scheduler.

**The residual cost is the return leg.** `wake_fault_owner` uses
`wake_task` + `set_next_synchronous_pick_hint` - a hint, not a handoff - and
pagerd issues `fault_reply` and `fault_wait` as two separate syscalls. seL4
merges these into `seL4_ReplyRecv`, which replies and blocks in one operation
so the woken caller can be switched to immediately. Merging them is the next
paging optimization; the desktop refresh still shows 313-427 ms in some runs
and nothing at all in others, so the remaining cost is bimodal and probably
placement-dependent.

### Closed: the pager was a client of its own transport

`broker_map_anon` demand-backed *every* anonymous mapping, with no exclusion
for the pager. Had pagerd ever taken an anonymous first-touch fault it would
have parked on a fault only pagerd can resolve, and every later fault in the
system would have stalled behind it - the classic external-pager self-deadlock
that L4-family and MINIX designs avoid by construction.
`handoff_pager_fault_to_waiter` rejects `receiver_slot == sender_slot`, so the
handoff would simply not happen and pagerd would stay blocked forever.

Nothing enforced the exclusion. It was avoided only by pagerd happening to be
`no_std` with fixed-size state, which is an accident, not an invariant. The
broker now excludes the pager-policy owner from demand admission and
`formal/check-performance-contracts.sh` pins it.

The owner is published once, at service-endpoint registration
(`PAGER_POLICY_OWNER`), and read with one relaxed atomic. The first
implementation took the service-endpoint registry lock on *every* anonymous
mmap; that is a hot path, and the fps gate dropped to 1 of 4 until the lock was
removed. Do not reintroduce a registry acquisition there.

### Closed: fault tokens and reply handles shared one ledger key space

The IPC donation ledger is keyed by a bare `u64`, and the new pager path bound
donations into it using the **fault token** while every other caller uses a
**reply handle**. The two encodings overlap numerically:

- reply handle = `(generation << 16) | (index + 1)`, smallest value `0x1_0001`
- fault token  = `(generation << 8) | slot`, slot `1..=128`

`0x1_0001` is therefore both the smallest reply handle *and* the fault token
for slot 1 at generation 256 - and slot 1 is reused on nearly every fault, so
generation 256 arrives well within a single boot. An aliased lookup settles
another subsystem's donation.

The ledger entry now carries a `DonationNamespace`, and every lookup matches
the pair rather than the number.
`a_fault_token_never_aliases_an_equal_reply_handle` pins it. That test caught a
real gap in the fix itself: binding a *reservation* set the key but not the
namespace, so the entry stayed in the wrong space.

### Open: `cancelled reply returned stale scheduling-context custody`

An intermittent kernel panic at `ipc_ops.rs:3298`, on the reply **cancel** path
(`cancel_endpoint_call_with_transfers` returns custody that
`settle_ipc_reply_scheduling_context` then refuses). Roughly 3 occurrences in
26 8-vCPU runs; it did not appear in 10 runs taken before this session's
changes, but that sample is too small to attribute confidently.

What is ruled out: it is **not** the donation-ledger aliasing above (it
survives that fix), and it is **not** the mmap registry lock (it survives the
lock's removal). It reaches the cancel path, so it needs a service call to time
out first, which is why it only shows under 8-vCPU load. The scheduling-context
custody store is separate from the donation ledger; note that
`BORROWED_CONTEXT_REPLY[receiver_slot]` is still keyed by a bare reply number
and is a second, narrower aliasing surface that was deliberately left alone.

### Open: pagerd is a single serial worker

`fault_wait` -> resolve -> `fault_reply` runs on one thread, so faults from all
CPUs serialize through it. MINIX's VM server is single-threaded too, but seL4
passive servers and QNX servers normally use worker pools. It does not affect
the current gate, whose faulting workload is one sequential thread, and it caps
throughput under multi-threaded fault load.

### Open: pager donation is weaker than the reference model

seL4 MCS donates the caller's scheduling context on `seL4_Call` and returns it
on `seL4_ReplyRecv`, so the server runs on the client's budget; QNX boosts the
receiving server thread to the client's priority. The pager path instead
applies a one-shot vruntime floor from exception context
(`apply_blocked_pager_donation`, which cannot take the donation ledger lock)
and binds the durable donation later, in the waiter syscall. pagerd's CPU time
is therefore not charged to the faulting task, there is a window between the
two stages, and there is no "boost to the highest waiting client" rule. This
matters for real-time claims, not for the current gate.

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

`cargo fmt --all -- --check` clean, `cargo xtask check`, `cargo xtask build`,
and `bash formal/verify-all.sh --profile pr` **sealed** at 44 artifacts, with
the TLC profile at 32 models and spec mutations at 183/183 killed. Host tests:
kernel-ps 270, kernel-compat 164, kernel-hal 66, kernel-mm 46, xtask 149,
rustos-user-abi 57, pagerd 28.

**KVM evidence is blocked, and not by this change set.** Every `kvm-smoke` run
on this checkout panics in early boot:

```
kernel/executive/src/boot.rs:267
no validated monotonic clocksource (invariant TSC or 64-bit HPET); acpi_hpet=None
```

This is before any pager, scheduler, or user code runs. The RustOS guest is
launched with `q35,accel=kvm,hpet=on` and `-cpu host,-x2apic,+invtsc`, so both
candidate sources are requested and neither is being accepted -
`hal::arch::clock::init` finds no ACPI HPET table and no usable invariant-TSC
frequency. Reproduced twice at 1 vCPU, and **a clean `HEAD` control build shows
the same failure**, so it predates this work. Until it is fixed there is no
runtime evidence for anything: treat every KVM claim on this checkout as
untested rather than as passing or failing.

That leaves one gap in this change set that only a boot can close: the
end-to-end partial-unmap probe (`mmap_split_survives_3_pages`, now in
`ipcbench`'s default set) has never executed. It mmaps three pages, faults them
all in, unmaps the middle one, and re-touches both remainders - which is
precisely what used to kill the process. It is compiled and shipped; it has not
run.

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
