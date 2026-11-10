# lifecycle area notes (task creation and termination)

Working notes for the `lifecycle` agent of the scheduler/IPC overhaul. Durable
results go into `docs/ai/ownership/terminal-activation.md`,
`docs/ai/ownership/identity-cleanup.md` and `docs/ai/smp/scheduler.md`.

## Design (one page)

Creation and termination are both custody transfers of one slot. Each one
must reach a single, observable end state, and no path may leave a half-made
publication behind:

- **Creation.** The steps are slot reserved (Ticket) -> hidden descriptor
  committed -> owner `Dormant -> Blocked` (`admit_blocked`, constructor ACK) ->
  exactly one wake publication (`publish_local_wake` / `publish_remote_wake`)
  -> notification of the owner of the queue (the IPI on the mailbox 0->1 edge,
  or the local safe point) -> optional first-turn ordering record -> first
  dispatch. The transition into a runnable owner is the enqueue duty (I1).
  `Published` puts the task in the target run queue or mailbox, and the
  mailbox edge winner owes exactly one IPI. A first-turn record is ordering
  metadata only. It may never be what lets the task, or any other task, be
  dispatched.
- **Termination.** One exact `Producer` loan per slot (retirement_cleanup row)
  is reserved, then either withdrawn and published, or parked in exactly one
  of three resumable origins: `Deferred` (withdrawn: owner `Retired`,
  lifecycle retired), `WaitingExecutor` (untouched, behind a kernel executor
  resume loan) or `Parked` (untouched, behind another CPU's in-flight dispatch
  claim). A claimant restores the exact origin on rollback. The origin decides
  what the claimant may assert: only a `Deferred` producer has withdrawn
  anything.
- The origin is one typed value (`Origin`), not a set of booleans. The
  housekeeping drain checks that the origin and the observable state agree:
  `Withdrawn` means the lifecycle is retired, and `ExecutorWait` / `Parked`
  mean it is not.

## Findings

### A. `terminal_publication.rs` "deferred retirement lost its terminal lifecycle or kernel resume loan"

Evidence: lane2 run 30 of round 2
(`4vcpu-100-round2-2026-11-09/lane2-1794228576359-1397497.log:453`). It came
right after `rootd: authenticated service health timeout`, so rootd was
terminating a service process: group exit -> `terminal_publication::guarded`.
Cause: `park_unwithdrawn` (a stop behind another CPU's in-flight dispatch
claim) recorded the producer as `Deferred`, the same phase as a withdrawn
stop. The housekeeping drain then claimed it and asserted
`lifecycle.retired || waited_for_executor`. Neither held, because nothing had
been withdrawn.

Review of the in-tree `Parked` fix:
- Parking is untouched at both park sites (`terminal_quarantine::publish` and
  `terminal_publication::guarded` via `retain_for_finalization`).
  `withdraw` returns false only when `retire_unless_claim_in_flight` returned
  `ClaimInFlight`, and that happens before any CAS of that iteration and
  before `begin_effects`. A previously withdrawn producer has an owner word of
  `Retired`, which returns `Retired` and never `ClaimInFlight`. So a withdrawn
  producer can never be parked.
- Resumption is correct for every claimant (drain, group exit, own exit or
  fault, terminate, wake/IPC quarantine, exec quiescence). Each one restarts
  `withdraw` from the top, and parks again while the claim is still in flight.
- Latent defect in the fix: origin was tracked as two booleans
  (`waited_for_executor`, `unwithdrawn`) plus `claimed`. `retain_stopped`
  (after a successful withdrawal) did not clear `waited_for_executor`, so a
  retained, withdrawn producer that was dropped would have been restored as
  `WaitingExecutor`. No current path drops a retained producer, so this is
  latent. Replaced with a single `Origin` enum (see "Done").
- The drain assertion now checks both directions (see "Done").
- Model: it matches the code for the three origins. It did not model the
  rollback of a claimed producer (Drop restores the origin) or a re-park from
  the drain. Both were added.

### B. A newly created task makes no progress (dominant boot family)

Reproduced root cause candidate (CPU-wide stall, not a lost enqueue):
`dispatcher::first_turn` returns `Incomplete` without consuming the front
first-turn record when the hinted task is not queued on this CPU. `choose`
propagates `Incomplete`, and `dispatch` then dispatches the IDLE task on every
turn of that CPU until the hinted task is `Local` there again. Every other
task queued on that CPU starves, and only an idle-CPU pull rescues them, one
per turn. Ways in:
1. The idle-CPU pull (`dispatcher::maintenance::service` ->
   `rehome_queued_frame`) steals the freshly activated task before its target
   CPU dispatches, and the record stays on the target.
2. Remote activation: the target drains the mailbox and fair-dispatches the
   task before the activator enqueues the record, which comes after the wake.
   The task then blocks.
3. The task is claimed by another CPU. `discharge_claimed_first_turn` only
   clears the claiming CPU's queues.

Test (fails before the fix, panics "actual policy observation incomplete"):
`activation/tests.rs::a_first_turn_record_for_a_task_no_longer_queued_here_never_idles_the_cpu`.
`dispatcher.rs` belongs to `dispatch`, who has been told the mechanism and the
suggested fix.

Creation-path audit (slot -> first dispatch):

| Path | Runnable transition | Enqueue | Wake/IPI | Notes |
| --- | --- | --- | --- | --- |
| root spawn (`root_creation::construct`, not suspended) | `publish_local_wake` / `publish_remote_wake` from `Blocked` | run queue or mailbox | IPI on `Published{notify:true}`, flushed by `allocate` | `set_next_spawn_pick_hint` (a hint arm that consumes the hint, so not sticky) |
| root spawn suspended + `activation::activate` | same, in `activation/publication.rs` | same | IPI to every accepted remote owner after the release | a first-turn record goes to `target` even when the wake was `AlreadyOwned` elsewhere (fixed: the record goes only where the wake published) |
| kernel task (`kernel_creation::publish_reserved`) | same | same | IPI on `notify:true`, flushed by `allocate` | |
| thread (`thread_creation::work` -> `commit`) | suspended `Blocked`, then activation | via activation | via activation | `submit` did not kick housekeeping, so a user creator waited up to one housekeeping nap (fixed: kick) |
| bootstrap / AP idle | `admit_initial` `Blocked -> Running` | none (Current) | n/a | one-shot |

Mailbox edge protocol (record under the mailbox lock, 0->1 CAS after it; the
consumer pops all and clears under the lock): an edge winner always exists
for an undrained record, and every winner sends an IPI. Reviewed, no lost
wakeup found. The deferred IPI (`TARGET_RESCHEDULE_IPI_PENDING`, used when
preemption is off) is flushed by every creation entry point.

## Done

- A: `producer::Origin {Withdrawn, ExecutorWait, Parked}` replaces the
  `claimed` / `waited_for_executor` / `unwithdrawn` booleans. Claim derives it
  from the row phase. `retain_stopped` sets `Withdrawn` (this fixes the latent
  rollback to `WaitingExecutor`), `park_unwithdrawn` refuses a withdrawn
  origin, Drop restores `origin.phase()`, and `drain_guarded` asserts
  `lifecycle.retired == origin.withdrawn()`. `resumed_unwithdrawn` was
  removed; `waited_for_executor` is now `cfg(test)`. Verdict on the in-tree
  fix: kept and completed. Its mechanism (a distinct untouched park phase) is
  correct; the boolean bookkeeping was replaced.
- A model: `Rollback` action, `DeferredMeansWithdrawn` invariant, drain
  assertion in its new two-sided form. The TLC run passes (102 states). All 4
  spec mutants are killed (one re-pointed, one new).
- B: the dispatcher consumer fix was landed by `dispatch`
  (`first_turn_record_is_stale` / `runqueue::queued_on`). The producer
  restriction in `ordering_publication::spawn_in` (Local only) was landed by
  `owner`. Activation now records a first turn only on its own `Published`
  wake. Regression test above (failed before the fix, passes now).
- Thread creation: `work::enqueue` kicks housekeeping. Test
  `thread_creation::work::tests::submitted_thread_row_kicks_the_housekeeping_executor`.
- Dead code: `current/group_exit::request` (no caller), a wrong `cfg` on the
  exec publication architecture import (`not(test)` -> `rustos_boot_image`).
- Registry: implementation mutations re-pointed (`kernel-executor-stop-claim`,
  `scheduler-outgoing-image-terminal-fence`,
  `terminal-producer-final-retained-rollback`,
  `scheduler-terminal-drain-assertion-forgets-parked-stop`,
  `scheduler-terminal-parked-stop-is-not-claimable`) and added
  (`scheduler-terminal-retained-stop-keeps-claim-origin`,
  `scheduler-terminal-withdrawn-stop-parks-as-untouched`,
  `thread-creation-row-leaves-housekeeping-unkicked`). All 8 are killed when
  run by hand in a private copy, because the runner's preflight is blocked by
  other areas' stale anchors. SMP source tokens re-pointed (rows 51, 1907,
  1910, 3221, 3233, 3236, 3303, 3337). Conformance token, CONFORMANCE row and
  owner contracts updated.

## Disconnected / unverified

- No KVM boot. Whether the first-turn stall is the whole "new task makes no
  progress" family is inferred from mechanism plus test, not proven on
  hardware. Next boots should show no idle census while a queue is non-empty.
- `legacy_witnesses` files (thread_creation, exec quiescence) are not
  duplicates. They are named witness bodies pinned by
  `check-dispatch-source-contracts.py` and called by historical test names
  that mutation rows use. Renaming them is churn and was not done.
  `tests/part_1.rs::collect_process_sibling_slots_returns_matching_user_slots_only`
  runs the same two bodies as `exec_transaction/tests/census.rs`. Not
  deleted, because a mutation row names it.
- `check-dispatch-source-contracts.py` stops at an owner-area failure
  (OWNER_WORDS moved), so later checks of my files were not reached.
