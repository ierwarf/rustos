# Dispatch area notes (overhaul working file)

Owner: `dispatch` agent. Scope: dispatcher/**, local_dispatch/**, dispatch_policy.rs,
dispatch_priority/**, runqueue_policy.rs, scheduling_context/**, scheduling_runtime/**,
execution_interval/**, locality.rs, *affinity*, stack_resources/**, runtime_profile/**,
smp.rs, irq_entry_tests/**, cpu_local/**, irq*.rs, current_identity/**; plus (granted by
the lead for the missed-wakeup fix) deferred_wake.rs and housekeeping_kick.rs.

## 1. Map: how a CPU decides what runs next

Entry points (all in `irq.rs`, each opens one `DispatchEntry` = one masked
`CpuDispatchTurn`, `cpu_local/irq_entry.rs`):

| Route | Gate before the scheduler | Request bit |
| --- | --- | --- |
| Timer (PIT on CPU0, LAPIC deadline on APs, re-armed in `complete_clockevent` on every exit path) | `preemption_disabled` -> publish local request, return; `retain_periodic_continuation` fast return; `timer_interrupted_kernel_frame` (user task in kernel mode is never preempted: request republished) | publish + claim after `DispatchEntry::enter` |
| RTC (CPU0) | same as timer | same |
| Reschedule IPI | `reschedule_ipi_gate`: preemption disabled -> RetainRequest (bit stays), no request -> AcknowledgeOnly | claim before `DispatchEntry::enter` |
| Software `int` (yield / block / IPC call) | none | publish + claim before entry (lane switch: no turn, no claim) |
| Syscall tail (`claim_interruptible_syscall_reschedule`) | request bit or user-return flag | claim, then `saved_user_context_dispatch` |
| `reschedule_if_requested` / `cond_resched` | preemption disabled -> nothing | claim, then `yield_now` |

`DispatchEntry::from_scope` (every route): settle a stashed fast IPC reply
(`pending_reply`), drain the CPU's deferred-wake batch (now until empty), service
`wake_retry`. Then `dispatcher::dispatch`:

1. `prologue`: account the outgoing interval, publish its frame, publish its queue
   return (`publish_queue`); an outgoing task whose affinity excludes this CPU is
   re-homed (`affinity_target_cpu` + local/remote wake + targeted IPI).
2. `choose` (priority chain, one `SELECTION_NOW` clock sample per turn):
   mailbox drain (`runqueue::drain_remote_wakes`) -> atomic activation FIFO
   (`first_turn(atomic)`) -> synchronous IPC FIFO (`synchronous`) ->
   `maintenance::service` (ready-frame sweep, active balance) -> overdue System
   (`choose_overdue_system`, ready-age bound) -> spawn FIFO (`first_turn`) ->
   latency hints -> next-pick hint (only when current blocked) -> user
   reservation (System streak bound) -> next-pick hint -> fair scan
   (`choose_local_next_with`, CFS vruntime, class strict, min granularity, burst).
   `Incomplete` from any arm ends the chain.
3. `claim`: `DispatchFrame::from_selection` -> `prepare` (budget funding) ->
   `claim` (owner CAS). Refusal: `claim_fallback` re-claims the outgoing task if it
   still has a current-local frame, else the CPU's fixed idle task.
4. `complete`: streaks, first-turn discharge, profile, lockdep witness.

Fast routes: `fast` (sync FIFO only, outgoing blocked) and `fast_switch`
(DirectSwitch frame loan) skip the chain.

How an idle CPU learns that work exists: the idle task is `loop { hlt }` with
IF=1 (`spawn.rs secondary_idle_entry`). It never checks queues. Work reaches it
through (a) a targeted reschedule IPI on the request bit's 0->1 edge
(`request_target_reschedule`; deferred through `TARGET_RESCHEDULE_IPI_PENDING`
while the sender holds a raw lock, flushed at listed safe points), or (b) its
periodic tick, which enters the scheduler unless `retain_periodic_continuation`
proves nothing is pending: `local_dispatch_work_pending` (fair queue count,
mailbox, wake_retry, sync FIFO), the request bit, the user-return flag, and now a
stashed fast reply. The tick is the universal backstop: every coalesced or
dropped IPI, deferred flush or `Incomplete` turn is retried within one tick as
long as the pending work is visible to that check. The liveness question for
this area is therefore "is every form of pending work visible to the skip
checks, and does every turn that sees work make progress".

## 2. Defects found (fixed in this batch)

D1. Deferred wakes invisible to every skip-the-scheduler check (missed wakeup).
`wake_task`/`wake_user_task` under a raw lock or inside a `DispatchPreemption`
scope append to `deferred_wake` and published nothing. Drained only by
`DispatchEntry`. Timer fast return and syscall tail consult only the request bit,
so a CPU running one compute-bound task (or idle after a leaf-holding IRQ
deferred) kept the wake indefinitely. Fix: `deferred_wake::defer_and_request`
publishes the CPU's own request after every append (`irq::request_local_reschedule_on`:
one CAS loop on the CPU's cache-padded request word + lock-free lifecycle
snapshot; no lock, no allocation, no lock order; legal under raw leaves, with
IRQs masked and in a DispatchPreemption scope). Drain side: every claim of the
bit is followed by a `DispatchEntry` (IPI/software/syscall tail claim before the
entry, timer/RTC claim after it); the entry now drains until the batch is empty
(also after `wake_retry::service`), so a deferral made by the entry's own wakes
after the first take cannot have its request claimed by the timer/RTC route
without being drained. Deferrals made later in the same turn (after the claim)
leave the bit pending for the next safe point. Tests:
`deferred_wake::tests::deferral_publishes_the_request_that_defeats_every_scheduler_skip`,
`..._racing_after_a_drain_and_claim_republishes_its_request`.

D2. Same blind-spot class: a stashed fast IPC reply (`pending_reply`) is settled
only at the replier's next syscall or scheduler entry; a replier that returned
to compute in user mode held the caller's wake indefinitely. Fix (consumer side,
the stash must keep riding the next receive): `retain_periodic_continuation`
treats `pending_reply::present()` as a pending request (one relaxed load of a
CPU-private cache-padded bool, timer path only). No host test: the timer gate
reads hardware-only CPU state; covered by reasoning + KVM (lead).

D3. First-turn FIFO front that no longer names a task queued here wedged the CPU
(reported and test-written by `lifecycle`:
`activation::tests::a_first_turn_record_for_a_task_no_longer_queued_here_never_idles_the_cpu`).
`first_turn` returned `Incomplete` without popping on every turn. Fix:
`dispatcher::first_turn_record_is_stale` (runnable, owner CPU == this CPU and
state Local/RemoteQueued/Migrating, else the record is consumed as `Empty`,
decrementing the atomic prefix count).

D4. Same class in the fair and overdue scans: one candidate whose admission never
resolves (zero/transitioning affinity snapshot, stale or CPU-excluded budget
stamp, borrowed donor without publication, winner whose frame keeps failing its
final proof) failed every observation of the whole set; the CPU then ran idle or
the outgoing task on every turn. Fix: `local_dispatch::UnresolvedCandidates` -
the first `MAX_LOCAL_FAIR_OBSERVATIONS - 1` observations stay strict, the last
omits exactly the slots unresolved in all earlier ones; epoch tears are not
slot-attributed and stay strict. Omission is reported by the
`scheduler-candidate-unresolved` milestone (arg0 turns, arg1
cpu<<32|reason<<16|slot). Tests:
`fair_retry::tests::only_a_candidate_unresolved_in_every_observation_is_omitted`,
`policy_witnesses::one_persistently_unresolved_candidate_cannot_idle_the_cpu`.

## 3. Hot-path cost (counts per operation)

- D1: deferral path only (rare): +1 CAS on the deferring CPU's own request line.
  Entry drain: +1 plain load of the CPU-private batch length per entry
  (`queued_masked`), no atomics. Dispatch hot path unchanged.
- D2: periodic tick only: +1 relaxed load (CPU-private, cache padded) + one
  `current_cpu_index`. Not on the IPC or dispatch path.
- D3: first-turn arm only when a hint is present: +1 owner word load.
- D4: fair/overdue scan: per candidate +1 stack bit test; no atomics, locks or
  shared lines on the success path. Degraded turn: 1 relaxed RMW (global
  diagnostic counter) + 1 store.

D5. Affinity could escape the scheduling context's `cpu_mask` (latent: rootd's
manifest uses `u64::MAX`). Root creation narrows the task mask to the budget
mask but not the process mask; `sched_setaffinity` / Windows affinity only
checked the process mask. Budget funding off-mask is an unresolved admission,
so the task sat runnable on a CPU that never ran it (and, before D4, wedged that
CPU). Fix: `affinity::funded_mask_admits` + `scheduling_runtime::context_cpu_mask`
in `thread_affinity::finish_with` and `process_affinity::Grant::capture`
(InvalidMask). Test: `affinity::tests::affinity_cannot_escape_the_scheduling_context_cpu_mask`
(pure predicate; the affinity fixtures are unbudgeted).

Other changes: per-CPU single-writer idle-fallback counter (was one global
`fetch_add` per dispatch to idle; `dispatch_claim.rs` call site edited with
owner's authorization); owner's `runqueue::conservation::scan` wired into the
timer/RTC scheduler entry (`runqueue_policy::custody_conservation_scan`).

## 4. Cleanup done

Deleted host-only "catalog" fixtures with no production counterpart and their
tests: `local_dispatch::{empty_fair_idle_generation, catalog_fair_fallback,
catalog_min_granularity_fallback, qualified_catalog_idle_generation}`,
`fair_retry::{retry_incomplete_fair, complete_hint_generation}`. Mutation rows
that mutated only those fixtures (proved nothing about the kernel) deleted:
scheduler-catalog-idle-foreign-owner-admitted, scheduler-empty-idle-foreign-cpu-accepted,
scheduler-empty-idle-owner-generation-zero-bypass, scheduler-fair-catalog-bypass-{always-rescans,drops-incomplete-fallback},
scheduler-fair-catalog-granularity-{always-rechecked,drops-incomplete},
scheduler-hint-incomplete-admits-catalog. Also deleted
scheduler-fifo-published-identity-accepts-incomplete: with D3 its mutant
(`is_none_or`) only drops an ordering hint for a torn-publication slot, which is
now consumed as stale anyway. Re-pointed: scheduler-fair-retry-reduced-to-one-observation
and system-flows `retry-incomplete-local-fair` -> `incomplete_early_policy_never_means_no_due_peer`;
conformance witnesses of SchedulerLocalAuthority -> the two D4 tests;
concurrency-triangle `scheduler-fair-retry-local-custody` -> `retry_incomplete_policy`.

## 5. stack_resources assessment (evidence: production callers per layer)

context 63, cpu_leases 42, task_start 53 (lock-free start mirror, hot IPC
identity check), frame_readers 19 (reader exclusion vs claim CAS), admission 18
(prepublication reservation phases), frame_access 8 (exclusive frame writer for
exec/MM), activation 4 (MM activation credential captured before claim), reuse 3
(reaper's slot-vacancy transaction). Every layer carries a production invariant;
none is pure scaffolding. The scaffolding is the parallel `#[cfg(test)]` API:
`context::{ExecutionRootLease, lease_execution_root, lease_for_architecture,
lease_root_inner, execution_root_is_current}` (a test-only reimplementation,
containing a dead `#[cfg(not(test))]` block, 22 test uses, 3 contract pins),
`stack_resources::close_for_reclaim`, `reuse::{begin, recover_detached_fixture}`.
Mutation rows that mutate that test-only code: mm-execution-architecture-lease-pin-drop,
mm-execution-kernel-root-exception-user-bypass, task-frame-close-writer-drain-bypass,
plus smp.rs (smp-foreign-owner-and-flip, smp-quiescence-or-flip,
lifecycle-execution-quiescence-ignores-unpublished-claim), banks.rs commit
(lifecycle-execution-duplicate-ack-spends-current), scheduling_context `allows_cpu`
(scheduler-scheduling-context-cpu-mask), scheduling_runtime `charge_domain_runtime`
(scheduler-scheduling-domain-shared-ledger), runqueue_policy `fallback_slot_for`
(sched-fallback-claim-failure-keeps-current), local_dispatch
`direct_handoff_frame_generation` (selection-native-direct-allocation-bypass).
DONE (follow-up): the `ExecutionRootLease` / `lease_execution_root` /
`lease_for_architecture` / `lease_root_inner` / `execution_root_is_current`
copy (84 lines, incl. a dead `#[cfg(not(test))]` block) is deleted. `mm_tests`,
`activation_tests` and `exec_transaction::publication::tests` now drive the
production path: `activation::capture` through the one-line seam
`context::test_capture_activation` inside a modeled dispatch turn
(`cpu_local::with_test_activation_return`; the capture refuses outside an open
turn, exactly like the claim path), `cpu_local::retain_mm_activation` for the
retention, and `frame_readers::with_claim_activation` for "no claim while the exec
image is open". Rows: deleted mm-execution-architecture-lease-pin-drop (type
gone) and task-frame-close-writer-drain-bypass (mutated the seam; the production
`|| r.frame_writing` in `reuse::begin_qualified` is redundant with the pin/reader
gates in every host scenario, so no discriminating witness exists - recorded
gap); re-pointed mm-execution-kernel-root-exception-user-bypass to
activation.rs and mm-execution-bank-custody-bypass to the production
`retained_architecture` gate (occurrence 1, new witness assertion in
`irq_entry_tests::lane::park_receiver`). Kept as minimal seams, with reasons in
their doc comments: `stack_resources::close_for_reclaim` (production closes only
through `reuse::begin_retired`, which needs a retired-qualified identity that
native fixtures never publish; ~70 fixture teardowns in many areas use it),
`reuse::begin` (3-line wrapper over the production `begin_qualified` with
retired=false) and `reuse::recover_detached_fixture` (production has no reopen
route by design). Still a test/production divergence inside production code:
`activation::capture`'s `#[cfg(test)]` early `Some(None)` for queue fixtures
without MM.

## 6. Disconnected / unverified / deferred

- No KVM boot ran (forbidden for agents). D1/D2 effects on the boot failure
  family are unproven; the milestone `scheduler-candidate-unresolved` and the
  conservation scan are the evidence to look for.
- D2 has no host test (timer gate reads hardware-only frame state).
- Mutation `--only` runs blocked: the runner validates the whole registry first
  and fails on owner-area stale anchors (runqueue.rs / ipc_handoff.rs rows).
- `lane_entry_cause` dead in test builds only (production-only CLI path).
