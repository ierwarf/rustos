# owner area: task ownership (design + status)

Working notes of the `owner` agent. Durable content moves to
`docs/ai/smp/scheduler.md` (section "Owner transition table") when the work lands.

## Design (one page)

**Module.** `scheduler/runqueue/ownership.rs` owns `OWNER_WORDS`, the packed
word (`RunOwnerSnapshot` encode/decode), the generation rule, the transition
table, the in-place edit rules, the resident-cell hook and the flight recorder.
No other file (including the rest of `runqueue/`) can CAS an owner word: the
only writers are `ownership::transition` (custody), the in-place edit functions
(run intent / wait metadata) and `reset_before_publication` (boot).

**State x event table** (`ownership::TRANSITIONS`, const; `Wait` = whether the
wait reason/arm survives; `Cpu` = target CPU rule; `Duty` = what the caller owes):

| from | event | to | wait | cpu | duty |
| --- | --- | --- | --- | --- | --- |
| Dormant | Admit | Blocked | clear | none | - |
| Dormant | AdmitRunning (fixture) | Running | clear | target | - |
| Blocked | AdmitInitial | Running | clear | target | - |
| Blocked | WakeLocal | Local | clear | target | RunQueue |
| Blocked | WakeRemote | RemoteQueued | keep | target | Mailbox |
| Blocked | HandoffDirect | DirectHandoff | keep | target | HandoffFifo |
| Blocked | HandoffRemote | RemoteQueued | keep | target | Mailbox |
| Blocked | LaneClaim | Running | clear | target | ResidentTake |
| Local | Claim | Running | clear | target | - (rq removal under the held rq lock) |
| DirectHandoff | Claim | Running | clear | target | - |
| Running | ReturnLocal | Local | clear | target | RunQueue |
| Running | ReturnBlocked | Blocked | keep | none | - |
| RemoteQueued | Adopt | Local | clear | target | RunQueue |
| DirectHandoff | Materialize | Local | clear | target | RunQueue |
| DirectHandoff | RollbackHandoff (fixture) | Blocked | keep | none | - |
| Local / RemoteQueued / DirectHandoff | Rehome | Migrating | clear | target | Migration |
| Migrating | PublishMigration | RemoteQueued | clear | target | Mailbox |
| Blocked / Local / RemoteQueued / Running / Migrating / DirectHandoff | Withdraw | Retiring | clear | keep | - (caller publishes terminal) |
| Retiring | CompleteRetire | Retired | clear | none | - |
| Retired | Release | Dormant | clear | none | - |

Anything else panics with the event and the observed word ("not in the owner
transition table"). Every row advances the generation by exactly one (I3).

**In-place edits** (generation, state and CPU unchanged): run intent may be
raised only in `Running`, `Local`, `RemoteQueued`, `Migrating`, `DirectHandoff`
(states whose custody record the dispatcher can find) and lowered only in
`Running` and `Local` (a lowered `Local` keeps its rq membership). Wait
arm/reason edits are allowed in every live state. `Dormant`, `Retiring`,
`Retired` are never edited (I2).

**Effects and their order.** A successful transition returns `Transition
{ next, duty }`. Fixed order after the CAS: (1) resident cell of the left
generation settled (evicted by the hook, or for `LaneClaim` the `ResidentTake`
duty takes it); (2) flight-recorder store; (3) the duty: rq membership (inserted
under the rq lock before the CAS and rolled back on CAS failure, so the duty is
proof-checked against the locked queue), mailbox record + pending edge, FIFO
record / `DirectSwitch`, or migration publication; (4) frame-generation bind and
published-load update by the caller that holds the rq lock; (5) IPI by whoever
received `notify: true` (outside every leaf).

**I1.** A transition into a runnable custody state yields exactly one
`EnqueueDuty` (`#[must_use]`, not `Clone`). It is consumed only by the code path
that performs that enqueue (`run_queue_inserted`, `publish_mailbox`,
`publish_migration`, `handoff_recorded`, `take_resident`). Dropping it panics
with the slot, generation and kind: a lost enqueue becomes a loud panic instead
of a silent stall. **I2.** `Retiring`, `Retired`, `Dormant` change only by the
table's exact-value CAS (no in-place edit, no second withdrawal: a withdrawal
that observes `Retiring` waits for the in-flight completion). **I3.** The
generation is monotone and advances on every custody change; the packed range
exhaustion panics.

**Flight recorder.** The 56 spare bytes of each owner word's 64-byte line hold a
7-entry ring indexed by `generation % 7`; the CAS winner of a generation owns
its entry, so one relaxed store, no RMW, no extra cache line. Entry = new word
low 17 bits | event | actor CPU | generation low 16 | old state/CPU/runnable.
Depth is 7, not 8 (one line); no tick is stored on the hot path (a TSC read is
~28 ticks, ~4 transitions per IPC round trip); the scanner supplies ages.

**Conservation check** (`runqueue/conservation.rs`). `scan(now_ticks)` from an
existing periodic path; one CPU at a time (try-flag), at most every 256 ticks.
Per slot it classifies a custody gap: `Local` without published rq membership,
`RemoteQueued` with no pending mailbox edge, `DirectHandoff` with no pending
FIFO, `Migrating`, `Blocked` with run intent, `Running` with no CPU execution
owner. A gap whose exact word is unchanged for >= 2048 ticks (2 s) is reported
once per (slot, word) as milestone `scheduler-custody-lost` (arg0 = slot | gap
kind << 8, arg1 = raw word) followed by up to 7 `scheduler-custody-ring`
milestones; at most 4 reports per scan, 32 per boot. No panic, no repair.
xtask classifies `name=scheduler-custody-lost` as its own failure kind.

**Resident cell / DirectHandoff.** Kept (see report): the lane is the
41.9k -> 14.4k win and resumes only through the resident cell; DirectSwitch was
measured at -11% (36.0k -> 31.9k). Both now go through the table
(`LaneClaim` + `ResidentTake` duty; `HandoffDirect` + `HandoffFifo` duty).

## Migration plan

1. Add `ownership.rs`; move the word there; route every runqueue CAS through
   `transition`. Old `pub(super)` entry points stay as wrappers (done in-module).
2. New cross-area API: `claim_blocked_handoff_duty`, `lane_claim`; old
   `claim_blocked_handoff` / `lane_cas` / `lane_running` stay as wrappers until
   `ipc` migrates.
3. Delete wrappers when `rg` shows no caller.

## Custody site inventory (step 1)

Writers (all now behind `ownership::transition` / in-place edits):
`runqueue/admission.rs` (Admit, AdmitInitial, AdmitRunning), `runqueue/claim.rs`
(Claim x2), `runqueue/return_publication.rs` (ReturnBlocked x2, ReturnLocal,
WakeLocal), `runqueue/ipc_handoff.rs` (HandoffDirect/HandoffRemote,
WakeRemote, Adopt, Materialize, RollbackHandoff, Rehome x3, PublishMigration),
`runqueue/lane.rs` (LaneClaim), `runqueue.rs` (Withdraw x2, CompleteRetire,
Release, fixture publish_local/publish_blocked), in-place edits
(`set_runnable`, wait arm/reason, `commit_wait`, `wake_wait_locked`) called from
`runqueue/wait.rs`, `current_return.rs`, `ipc_lane.rs`, `terminal_*`,
`activation/publication.rs`, `exec_transaction/publication.rs`. Resident cell:
`resident::{park, take, on_owner_transition, commit_outgoing*}`; queue/mailbox
membership: `RUN_QUEUES`/`REMOTE_WAKE_MAILBOXES` only inside `runqueue/**`;
FIFO: `sync_handoff::publish_original` (HandoffFifo duty). Readers: ~460
`runqueue::owner(slot)` snapshot reads (observation only; `queued_on`,
`is_*_dispatchable`, `wait_blocked` are the typed predicates).

## Status (2026-11-10)

Done: ownership.rs (private OWNER_WORDS, const table + compile-time O(1)
index, typed `transition`, `EnqueueDuty`/`Transition`, in-place rules, flight
ring), every runqueue CAS routed through it, wrappers `claim_blocked_handoff`,
`publish_handoff_mailbox`, `lane_cas`, `lane_running` deleted after ipc
migrated; conservation scan wired by dispatch (irq.rs timer/RTC dispatch);
xtask failure kind `scheduler custody lost`; contract `docs/ai/smp/scheduler.md`
7.9/7.10; registries re-pointed (implementation-mutations, smp-source-contracts,
check-dispatch-source-contracts.py, concurrency-triangle, system-flows,
run-source-conformance, contracts.toml source mapping, models.md row, TLA
source-map comments).

Unverified: no KVM boot (forbidden here), so the flight-ring/IPC cost claim is
counted, not measured; the conservation scan has never run in a guest; no TLC
run (models unchanged except comments).

Deferred: RemoteQueued gap uses the lock-free MAILBOX_PENDING proxy (a stale
pending=1 with the record missing is not caught); `publish_local_wake` does not
assert target == current CPU (all callers pass the current CPU).
