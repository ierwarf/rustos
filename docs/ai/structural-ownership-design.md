# RustOS Structural Ownership Design

**Status:** normative design for the audit items that remain structurally open
after `RustOS_Ring0_Ring3_Structural_Audit_v5.md`. It fixes the target
structure, the linearization point of every ownership transfer, the memory
ordering that publishes it, and the mutant each formal model must kill. It is
not evidence that any of it is implemented; per-item status lives in section 1
and is re-derived from source, never inherited.

Detailed wire rules stay in `contracts-abi.md`, proof infrastructure in
`contracts-infra.md`, and the SMP envelope in `smp-contract.md`. This document
owns *who may mutate what, and when the mutation becomes visible.*

## 0. Why this exists

The audit's own closure matrix was produced by source inspection rather than
counterexample reproduction, and it says so. Two of its entries were later found
to be misclassified from name-based searching, and a third — `V5-VFSD-HOL-007`,
recorded as closed — is contradicted by runtime: the executable snapshot times
out at 2005 ms under 8 vCPU and fails `storaged` spawn repeatedly.

So section 1 re-derives status from source, and section 1 itself then had to be
corrected once for the same name-based error. Everything after it is design for
what that check found still open.

A claim of closure here requires the construct to exist in source **and** a
mutant that fails without it. Source inspection alone is what produced the
matrix this document had to correct twice.

## 1. Verified status, re-derived from source

Checked against the working tree, not against the audit's matrix.

### Closed, with the source construct that closes it

| item | closing construct |
|---|---|
| `V5-SCHED-DONATION-002` | `IpcDonationTarget::{AwaitingReceiver, BoundWorker(task)}` in `scheduler/ipc_donation.rs`; receive commit replaces the prebind with the exact worker |
| `V5-SCHED-RESCHED-003` | packed `request_word` plus separate `notify_seq`/`consume_seq` in `multitask/reschedule_observation.rs` |
| `V5-IPC-STREAM-004` | `commit_send(accepted)` in `user/socket.rs`, with a test that a partial send advances only the provider-accepted range |
| `V5-DVM-LIFECYCLE-005` | `TransportState::Activating` and packed `(epoch, state)` CAS in `io-manager/transport_lifecycle.rs` |
| `V5-DVM-INPUT-MAP-006` | `TransportMappingClaim` gating every `SHARED_ADDR` load in `io-manager/input/dvm_ring.rs` |
| `V5-WAITSET-HERD-008` | `wake_matching(provider, Some(object_id), Some(generation))`; the provider-wide form is reached only from `revoke_waitset_provider`, which is the terminal HUP the design requires |
| `V5-UI-CLASS-010` | `spawn_ui_thread(UiThreadRole, ...)` is the only spawn path in `uiserver/sys.rs` and `uiserver/app/runtime.rs` |
| `V5-FORMAL-RESCHED-016` | `SmpRescheduleIpi.tla` carries `requestSeq`/`notifySeq` and a claim action; the vacuous `selfIpiSent` invariant is gone |
| `V5-FORMAL-DVM-017` | `DvmTransportLifecycle.tla` carries `Activating`, `claims`, and `activationCancelled` |
| `V5-FORMAL-EXEC-020` | `publicationFailed' = TRUE` is reachable from an action in `ExecAddressSpaceTransaction.tla` |
| `V5-IPC-AUTH-015` | `bind_ipc_transfer_receiver_by_tickets` pins `receiver_open_description` at bind and `claim_ipc_transfer_entries_by_tickets` requires the identical description, so dup, fork, or close inside the receiver set cannot move a bound batch |
| `V5-FORMAL-IPC-018` | `IpcTransferAuthority.tla` sets `receiverSetEpoch` at `BindReceiver` and requires `receiverEpoch = receiverSetEpoch` at `Claim`; `PartialOrdinarySend(requested, accepted)` models provider-accepted length separately from requested |
| `V5-DEADLINE-012` | `rustos_user_abi::deadline::AbsoluteDeadline` is the one shared arithmetic; inputd, loaderd, and vfsd derive every child timeout and retry sleep from one end instant, and the snapshot wire carries it. Three mutants fail without it |
| `V5-MM-STACK-DEAD-021` | there is no lazy-growth route to delete. `USER_STACK_INITIAL_COMMIT_PAGES + USER_STACK_GUARD_PAGES == USER_STACK_RESERVE_PAGES` is a `const` assertion, no fault path grows a stack, and `release_stack_maps_every_usable_page_above_one_guard` pins `reserve_start == committed_start` |

### A correction, recorded because the mistake is instructive

This document's first revision listed `V5-IPC-AUTH-015` as open and **reopened**
`V5-FORMAL-IPC-018`, on the grounds that `IpcTransferAuthority.tla` models a
`receiverSetEpoch` while `rg receiver_set_epoch` over `kernel/` and `libs/`
returns nothing.

That was wrong, and it was wrong in exactly the way this repository has been
burned before: searching for an identifier taken from the audit's *proposed*
design instead of reading the implementation. The property is implemented under
a different and stronger name. The audit proposes an epoch counter bumped
whenever the receiver set changes; the implementation instead pins the exact
receiving open description on first bind and refuses any other. Pinning is
stronger, because it does not depend on detecting *how* the set changed — dup,
fork, and close are all excluded by construction rather than by observing a
counter — and it needs no channel registry.

**Refinement map, so the next reader does not repeat this.** Model
`receiverSetEpoch` corresponds to source `receiver_open_description`; model
`BindReceiver` corresponds to `bind_ipc_transfer_receiver_by_tickets`; model
`Claim`'s `receiverEpoch = receiverSetEpoch` corresponds to the claim-side
equality on `context.receiver_open_description`. The names differ because the
model was written against the audit's vocabulary and the source against its own.
That divergence is a documentation defect, not an authority gap, and it is fixed
by writing the mapping down rather than by renaming either side.

### Open, and owned by this document

| item | what is actually missing |
|---|---|
| `V5-SCHED-GLOBAL-001` | **narrower than the item text, and now measured against a normalizer.** The per-CPU runqueue, owner words, remote mailboxes, and per-CPU selection already avoid the global scheduler lock. Dispatch frame/stack/SIMD/TLS/vruntime/affinity/root/weight plus blocked state, ready/block timing, wait arm, exact wait reason, and ordinary block commit live in owner-generation-bound per-slot storage. The block mirror, runnable bit, reason kind, and arm now share one owner word, so commit and a racing wake linearize through one CAS instead of the catalog. Synchronous-IPC call admission, receive-side donation bind, reply custody settlement, log-identity lookup, wait arm/cancel, and the normal block commit answer without the guard. Catalog acquisitions per scheduler entry moved 4.65 -> 2.59 -> **1.99** at one vCPU; the previously measured eight-vCPU 3.27 -> 2.02 predates the commit cut. What still enters the guard on the ordinary path is dispatch itself, the reply wake handoff, pick hints, retirement producer/reaper and side-effect detachment; external cleanup claim/ACK now uses independent per-slot cells. Scheduling-context budget/custody and lifecycle/directory metadata still sit behind one `TrackedSpinLock`; ordinary-path acquisition zero is therefore still open. Section 2 |
| `V5-FORMAL-SCHED-019` | `SchedulerCpuOwnership.tla` models the guard, not the removal of the guard. Section 2.7 |
| `V5-VFSD-HOL-007` | **structure landed, runtime evidence owed.** The receive owner never blocks, the plan carries its mount generation and the commit refuses a stale one, and custody is a bounded two-worker pool. What is still owed is the measured control-lane residence bound in section 3, and the 2005 ms snapshot itself is still unattributed. Section 3 |
| `V5-WAYLAND-HOL-013`, `V5-GPU-UI-OWNER-014` | no `WaylandProtocolOwner`, `SceneOwner`, `GpuSubmissionOwner`, or `FramePlan`. Section 4 |
| `V5-UI-PIPELINE-011` | `frame_seq` exists only inside `uiserver/main.rs` and `loop_timing.rs`; it never reaches the scheduler, IPC, or DVM relay. Section 5 |
| `V5-TLB-SCALE-009` | **structure closed, runtime evidence owed.** `active_root_targeting_is_sound` asserts CR4.PCIDE is clear per admitted CPU, and the unacknowledged-target policy is documented as system fail-stop with the reclaim alternative rejected in writing. What is owed is the measured ACK latency distribution under a delayed vCPU. Section 8 |

`V5-VFSD-HOL-007` being recorded closed while it times out is the reason section
1 exists. The correction above is the reason it re-derives from source instead
of from identifiers.

## 2. Per-CPU scheduler (`V5-SCHED-GLOBAL-001`, `V5-FORMAL-SCHED-019`)

### 2.1 What the measurement changed about the target

Three sessions of measurement have resized this item twice, and the design has
to reflect what is in the tree rather than the audit's framing.

**The audit's framing.** Dispatch selection is serialized by a global lock;
build a per-CPU runqueue.

**First correction, from the acquisition census.** The guard is taken about
eighteen times per dispatch, and the excess is wake, pick hints, donation,
affinity, and lifecycle traffic. The largest callers were read-only identity
queries and the syscall SIMD pair, both now answered outside the lock.

**Second correction, from reading `runqueue.rs`.** The per-CPU runqueue is not
missing. It is implemented, and already lock-free with respect to the global
scheduler lock:

- it owns a per-slot `RunOwnerWord` with the full state machine
  `Dormant -> Blocked/Local -> RemoteQueued -> Local -> Running`, plus explicit
  `Migrating`, `Retiring`, and `Retired` custody;
- `publish_remote_wake` CASes the owner word and takes only the *target's*
  mailbox lock, with a 0->1 edge granting notification custody to exactly one
  producer;
- `drain_remote_wakes` takes only the mailbox lock and the target's own rq lock;
- every selection, steal, balance, and locality path already iterates
  `local_runnable_slots(cpu)`. There is no global ready scan anywhere.

So what the global `SCHEDULER` lock still protects is neither the queue nor the
selection. It is the `Scheduler` struct itself: one `TrackedSpinLock` over the
remaining scheduling-context budget/custody and per-task lifecycle arrays —
`contexts` (scheduling context plus immutable ABI and process metadata),
`starts`, `retired`, and lifecycle flags. Dispatch and wait payloads
(`vruntime`, fair-share weight, frame, stack geometry, SIMD/TLS, affinity,
address-space root, blocked state, ready/block timing, arm, and exact reason)
are per-slot storage and no longer reside in that catalog.

**The remaining ownership work for `V5-SCHED-GLOBAL-001` is a data-structure
split; ordinary fair selection now also has an explicit local-policy owner.**
Per-task fields only the owning CPU mutates still leave the globally locked
struct for per-slot storage exactly as the writer table in 2.4 states. The
policy cutover is separate: generation-bound local publication owns ordinary
fair choices, while exact activation and IPC FIFO custody stay catalog-owned.

The former `TaskContext.ready` duplicate has already been removed. The split
must instead preserve the existing run-owner transition as the sole runnable
authority while moving only fields whose writer is provably the owning CPU.
That is a data-ownership migration, not a reason to weaken the lock or duplicate
state again.

### 2.1b Where the lock time actually goes, measured at 8 vCPU

Steady state, per one-second window, eight vCPUs:

| figure | value |
|---|---:|
| guard acquisitions | 143,000/s |
| dispatches | 29,400/s |
| acquisitions per dispatch | 4.9 |
| lock hold total | 600 ms/s — 60 percent duty on one lock |
| lock wait total | 950 ms/s |
| in-owner attributed | 545 ms of the 600 — 91 percent |

The 91 percent attribution matters: this is real critical-section work, not a
descheduled vCPU, so shortening the section is a real lever and the earlier
`CPUID`-style implementation win is not available again.

By segment: `select` is 284 ms/s, and inside it `SelectHandoff` alone is
238 ms/s — 40 percent of all lock hold, about 8 µs per dispatch. Everything else
is small: balance 70, validate 43, commit 85, arch-restore 38, prologue 19.

**One measured non-result, recorded so it is not retried.** `overdue_class_pick`
runs inside that segment, scans every local runnable slot per dispatch, and
called `slot_class` — which resolves the effective class through the donation
chain — before the cheap ready-age test. Reordering the conjunction so the
arithmetic age test rejects first moved the number by nothing: 8.2 µs per
dispatch before, 8.5 after. The predicate chain is not the cost.

The segment is now split three ways, by self-timing each scan rather than by
moving the phase marks. At 8 vCPU, 28,592 dispatches in one window:

| part of `SelectHandoff` (253 ms/s total) | cost | share |
|---|---:|---:|
| fair-class pick (`pick_min_vruntime*`) | 34 ms/s | 13% |
| overdue-class and reserved-user scans | 59 ms/s | 23% |
| **the handoff chain itself** | **160 ms/s** | **63%** |

So neither scan is the cost. The 63 percent is the `take_next_*_ready_slot`
sequence — atomic activation, synchronous pick hint, bootstrap handoff, latency
pick hint, ordinary pick hint — run in order on every dispatch at about 5.6 µs
per dispatch. That is the target, and it is not what the segment's name
suggested; the earlier predicate reorder failed because it optimised the 23
percent's inner loop.

### 2.1c A big lock is not automatically the problem

`V5-SCHED-GLOBAL-001` reads as "remove the global lock". The commercial
microkernel literature does not support that as a general rule, and the
measurement says which part of it applies here.

seL4 — a verified, commercially deployed microkernel — ships SMP with a **big
kernel lock** by choice. "For a Microkernel, a Big Lock Is Fine"
(<https://trustworthy.systems/publications/nicta_full_text/8768.pdf>) and its
longer evaluation (<https://arxiv.org/pdf/1609.08372>) argue that coarse-grained
locking is viable for a microkernel because its critical sections are extremely
short, and report it holding up to roughly eight cores for typical workloads.
The seL4 FAQ frames the same choice: the big lock suits cores sharing an L2, and
beyond that the answer is a multikernel rather than finer locking.

Two things follow for RustOS, and they point in opposite directions.

- **The core count is inside the regime that literature defends.** Eight vCPUs
  sharing a cache is precisely where a big lock is reported to be fine.
- **The critical section is not.** The workload the evaluation names as the one
  that breaks a big lock is high-frequency IPC, and that is exactly this
  system's profile: 143,000 acquisitions per second with 4.9 per dispatch. More
  decisively, a microkernel dispatch that is "fine" under a big lock is
  sub-microsecond; this one spends 8 µs in `SelectHandoff` alone, with 91
  percent of the hold attributed to in-owner work.

So the target is the critical section first, and the lock only if shortening it
is not enough. That ordering also has better failure modes: shortening a segment
is measurable per change and reversible, whereas the cutover is the step that
turns a disagreement into a task on two CPUs. The staged ladder in 2.7 stands,
but step 3 should be preceded by taking the 160 ms/s handoff chain apart — if
that alone brings the hold under a few percent duty, the remaining stages become
optional rather than mandatory.

### 2.1d What the handoff chain was actually spending, and what removed it

Taking the chain apart gave a result that no amount of reading the source would
have produced, and it is worth stating plainly because it changes what the
remaining stages are for.

Each of the six chain members was self-timed at its call site. Every one came
back between 0.95 and 2.5 µs per call, and the times did not track the work: the
step that pops an empty queue and returns cost 0.81 µs, while the step that
walks the local runnable set cost 0.86 µs. A cost that is flat across steps that
do wildly different amounts of work is not the work.

Two controls priced the alternative. An empty timed span read **0.032 µs**; one
bare acquisition of this CPU's dispatch policy read **0.724 µs**. That is the
whole explanation. The per-step costs fall out of the acquisition count exactly:
one acquisition per step for the blocking hint, two for atomic activation and
for the synchronous hint, three for bootstrap, and the two overdue steps are a
scan plus at most one acquisition.

The chain was taking the same per-CPU lock about ten times per dispatch.

Why an uncontended per-CPU lock costs 0.72 µs is not a mystery either. Every
`TrackedSpinLock` acquisition runs `disable_preemption`, the lock-order graph
walk in `before_acquire`, a TSC read, an `after_acquire` under
`without_interrupts`, and on release a second `without_interrupts` containing
`current_cpu_index`, `current_apic_id`, and the guard-ownership check. That is
three to four `RDTSCP` reads and two interrupt-flag save/restore pairs per
acquisition. The instrumentation is worth its cost once per critical section.
It is not worth it ten times inside one.

The fix keeps the ownership boundary exactly and pays for it once: the chain
acquires the current CPU's policy a single time and threads the guard through
every step. Cross-CPU pushers still acquire the target CPU's policy on their
own, which is the boundary `V5-SCHED-GLOBAL-001` needs to survive the cutover.

The first attempt at this deadlocked-by-assertion rather than silently, which is
the lockdep design working: `reserved_user_pick` reaches `user_reservation_due`,
which took the same policy lock a second time, and the guest panicked with
`recursive lock-class acquisition class=42` before boot reached rootd. Threading
the guard into that path too is what made it correct. The lesson is the one the
vfsd storage lock already taught — a second live guard on one lock inside one
call is the failure, and the only reliable way to find them all is to let the
class assertion do it.

Measured at 8 vCPU, per dispatch, same instrumentation before and after:

| step | before | after |
| --- | ---: | ---: |
| atomic activation | 1.42 µs | 0.039 µs |
| synchronous hint | 1.42 µs | 0.059 µs |
| overdue (mandatory) | 0.86 µs | 1.03 µs |
| bootstrap | 2.11 µs | 0.046 µs |
| blocking hint | 0.81 µs | 0.039 µs |
| overdue or hint | 1.52 µs | 1.00 µs |
| policy acquisition | — | 0.575 µs |
| **chain total** | **7.39 µs** | **2.50 µs** |

Two thirds of the chain was lock instrumentation on a lock that could not be
contended. What remains is 2.0 µs of genuine class scans and one acquisition,
and the two overdue steps are now the only members worth attacking — they are
the same `overdue_class_pick` walk charged twice per dispatch.

### 2.2 Reference designs and what each contributes

- **Linux scheduler domains** — <https://www.kernel.org/doc/html/latest/scheduler/sched-domains.html>.
  Each CPU owns a base domain; the hierarchy is built through `->parent`; a
  domain's span must be a superset of its child's span and a base domain for CPU
  *i* must span at least *i*; the union of a domain's group cpumasks must equal
  the domain span. Balancing is periodic per domain, triggered from the tick and
  performed in softirq, each domain having its own exhausted-interval test.
  **Taken:** the domain hierarchy, the span invariants, and per-domain intervals
  instead of one global scan.
- **FreeBSD ULE** — <https://github.com/freebsd/freebsd-src/blob/main/sys/kern/sched_ule.c>.
  Per-CPU `tdq` with its own mutex; fields are classified by who may write them
  (queue-locked, serialized-store/lockless-load, CPU-local-store, purely local);
  acquiring two queue locks orders them by address; `tdq_notify` IPIs the target
  when it differs from the current CPU; migration custody transfers only when
  the thread lands on the destination run queue.
  **Taken:** the per-field writer classification and the "custody transfers on
  landing" rule. **Deliberately stricter:** RustOS forbids holding two run-queue
  locks at all rather than ordering them, so no lock-ordering proof is needed
  (see 2.4).
- **Zircon fair scheduler** — <https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/docs/concepts/kernel/fair_scheduler.md>.
  "A thread may compete only on one CPU at a time"; per-CPU queues ordered by
  virtual finish time in a balanced tree; deadline work takes precedence over
  fair work with a guaranteed capacity budget.
  **Taken:** exact-one-competitor as the top invariant, virtual finish time as
  the fair order, and admitted-capacity deadline as a separate class.
- **Xen Credit2** — <https://xenbits.xen.org/docs/unstable/features/sched_credit2.html>.
  Exposes migration resistance and over/under balance thresholds as first-class
  tunables.
  **Taken:** migration cost is part of the load metric, not an afterthought;
  this matters more under KVM, where a migration also costs host-side cache.
- **seL4 MCS** — <https://docs.sel4.systems/Tutorials/mcs.html>.
  A scheduling context capability carries budget and period; a TCB and a
  scheduling context bind one-to-one; a passive server runs on the client's
  donated context; the reply object tracks the donation chain and returns the
  context.
  **Taken:** donation is owned by the reply object and is budgeted, not merely a
  priority bump. This is already how `ipc_donation.rs` binds an exact worker;
  the per-CPU design must not regress it.
- **Commercial INTEGRITY RTOS** — <https://www.ghs.com/products/rtos/integrity.html>.
  The vendor describes microkernel partitioning, fixed CPU/memory resource
  budgets and bounded kernel service times. **Taken:** admission/lifetime
  resources must be explicit and kernel work bounded. Assignment policy stays
  in RustOS userspace services; this public architecture page is not evidence
  of vendor locking, ABI details or equivalent performance. Together with the
  re-read seL4 MCS reply-object rules, this directs the next transfer toward
  pinned execution/funding leases, not a renamed global lock or a latest-donor
  lookup after return/cancellation.
- **QNX server boost** — <https://www.qnx.com/developers/docs/7.1/com.qnx.doc.neutrino.sys_arch/topic/ipc_Server_boost.html>.
  QNX states plainly that a boost applied because no thread is RECEIVE-blocked
  "is considered to be a symptom of a poorly designed server, and the kernel's
  response is an attempt to work around it," and that a server "should always
  have at least one RECEIVE-blocked thread."
  **Taken:** boost stays an exceptional path. Every RustOS daemon must keep a
  receive-capable owner; section 3 applies this to vfsd directly.
- **Rust `Ordering`** — <https://doc.rust-lang.org/std/sync/atomic/enum.Ordering.html>.
  A `Release` store synchronizes-with an `Acquire` load of the same location; a
  failed `compare_exchange` performs only the failure ordering; `AcqRel` never
  performs a relaxed access.
  **Taken:** every publication below names its store and its paired load.

### 2.3 Structure

```rust
#[repr(C, align(128))]
struct PerCpuScheduler {
    rq_lock: IrqSafeRawLock<LocalRunQueues>,
    current: CurrentOwner,
    fair_system: VTimeTree,
    fair_user: VTimeTree,
    critical_deadline: DeadlineTree,
    deadline: DeadlineTree,
    remote: IntrusiveMpsc<RunTransfer>,
    resched: ReschedWord,
    timer: CpuDeadlineHeap,
    load: SeqSnapshot<RunQueueLoad>,
}

struct TaskRunAuthority {
    /// generation | RunState | cpu-or-token. The single linearization word.
    owner: AtomicU64,
    transfer: UnsafeCell<RunTransferSlot>,
}

enum RunState { Blocked, RemoteQueued, Local, Running, Migrating, Retiring, Retired }
```

What stays global is a `TaskDirectory` of immutable identity, lifecycle
generation, address-space handle, affinity generation, and refcount. Runnable
ordering, current, vruntime, deadline, and mailboxes are never in it. Process
exit and exec take a lifecycle freeze token from the directory; they never edit
a remote queue.

### 2.4 Writer rules

Adopting ULE's per-field classification explicitly:

| state | who may write |
|---|---|
| local trees, `current`, vruntime, timer heap | the owning CPU, under its own `rq_lock` |
| fair-share weight and trusted base-class bit | lifecycle admission before owner publication; current owner for surrender-only demotion |
| `TaskRunAuthority.owner` | any CPU, by CAS only |
| `transfer` slot | the CPU that won the owner CAS, before publishing the mailbox entry |
| saved frame, kernel stack, FPU/SIMD, TLS, dispatch address-space root/mm-active bit | the `Running` owner CPU only |
| `TaskDirectory` rows | the lifecycle owner, under the directory token |

**Two run-queue locks are never held at once.** ULE orders them by address;
RustOS forbids the pattern instead, because a steal that needs the victim's lock
is replaced by a victim-mediated pull: the thief posts a pull request, and the
victim moves the thread at its own next safe point. That removes a lock-ordering
obligation from the proof at the cost of one extra hop, which is the right trade
when the whole point of the change is to stop CPUs waiting on each other.

Forbidden inside `rq_lock`: allocation, user copy, `ProcessState` lock, service
IPC, any sleeping lock, and TLB waits.

### 2.5 Linearization points and ordering

| transition | linearization | ordering |
|---|---|---|
| wake | `Blocked(g) -> RemoteQueued(dst, g)` CAS on `owner` | write `transfer` first, then `Release` the mailbox push; destination `Acquire` on pop |
| local claim | `RemoteQueued -> Local` CAS, then tree insert under `rq_lock` | `AcqRel` CAS |
| dispatch | tree removal and `Local -> Running(cpu)` in one `rq_lock` section | `Release` publishes `current` |
| migrate out | `Local -> Migrating(token)` CAS by the source CPU, then tree removal, then mailbox post | `AcqRel` CAS, `Release` post |
| migrate in | `Migrating(token) -> Local(dst)` CAS by the destination only | `Acquire` pop |
| retire | `* -> Retiring(token)` by the lifecycle owner; queue/current owner ACKs quiescence; `Retired` after TLB generation, donation, and IPC wait cleanup | `AcqRel` throughout |

Zircon's "a thread may compete only on one CPU at a time" becomes the standing
assertion enforced by the unique `CpuDispatchTurn::publish_return` producer;
the directory guard delegates to that owner and cutover must preserve it.

### 2.6 Placement and balance

Wake placement order: affinity mask, then exact handoff or idle destination,
then last CPU or cache cluster, then capacity and misfit, then least normalized
load. Idle pull is bounded by victim count. Active balance runs per topology
domain on that domain's own interval, following Linux, and includes migration
cost and cache locality in the metric, following Credit2. There is no global
tick scan.

`System` stops being an unbounded priority class and becomes a per-CPU reserved
budget and period, which is the seL4 framing. Deadline and critical work is
admitted by utilization, which is the Zircon framing.

### 2.6a Authoritative fair-policy contract

The local picker is an independent policy, not a bit-for-bit legacy emulator.
Its acceptance properties are:

1. **Eligibility before preference.** Lifecycle generation, CPU ownership,
   affinity, stable saved frame, effective donation class, and both context and
   domain budget must all be current. An unstable runnable/class publication or
   one failed candidate revalidation rejects the whole local observation.
2. **Exact custody wins.** Atomic activation and synchronous IPC FIFO handoffs
   retain catalog authority. Local fairness cannot reorder them.
3. **Bounded class service.** System work is preferred, but after at most
   `MAX_CONSECUTIVE_SYSTEM_DISPATCHES` while User work is eligible, the least
   virtual-runtime User choice is reserved. A reservation that finds no User
   candidate does not alter the following ordinary fair choice.
4. **Weighted fair order.** Within a class, the least weight-normalized
   `vruntime` wins. Equal keys are deterministic; voluntary yield excludes the
   current task on its first pass, and a task at maximum burst yields to an
   eligible same-class alternate. The catalog-owned User-reservation and
   voluntary-yield arms were temporary authority boundaries: once the catalog
   has selected a reserved User slot, or when the current task yields, local
   fairness falls back rather than replaying a possibly stale policy epoch.
5. **Bounded locality.** A same-CPU candidate may replace the class minimum only
   within `SCHED_CPU_LOCALITY_LAG_NS`; locality never crosses a class, budget,
   affinity, lifecycle, or custody boundary.
6. **Progress, not wall-clock fiction.** User progress is guaranteed by the
   bounded System/handoff bursts and virtual-runtime accounting. No unadmitted
   hard completion deadline is claimed. System recovery retains its existing
   bounded ready-age rail.
7. **Fail closed.** If no complete local decision exists, use the already
   computed catalog decision. A fallback is an authority boundary, not a policy
   mismatch.

This follows Linux EEVDF's separation of eligibility from virtual deadline/lag
(`docs.kernel.org/scheduler/sched-eevdf.html`), CFS's weight-normalized virtual
runtime (`docs.kernel.org/scheduler/sched-design-CFS.html`), FreeBSD ULE's
per-CPU queues and affinity/topology awareness (`sched_ule(4)`), and Zircon's
single-CPU competition invariant. RustOS deliberately retains its explicit
bounded System reservation and exact IPC custody instead of importing any one
scheduler wholesale.

Acceptance is property-based: no dual owner, no ineligible dispatch, bounded
User service under sustained System load, deterministic equal-key choice,
bounded locality lag, exact voluntary-yield exclusion, budget conservation, and
forward progress at 1/2/4/8 vCPU. Legacy disagreement is reported for diagnosis
and performance interpretation but is not itself failure.

### 2.6b Current authority and remaining deletion boundary

This section is the current contract, not a chronological build diary. Latest
validation belongs only in `docs/ai/session-handoff.md`; formal proof scope and
witnesses belong in `formal/{model-bindings.tsv,proof-index.toml,concurrency-triangle.toml}`.
Earlier working-tree boots and filtered catalog comparisons are historical,
not acceptance evidence for the current source. Legacy tie-break agreement is
not required; the policy and exact-one-custody invariants in §2.6a are.

#### Current deletion inventory (supersedes earlier migration-stage inventories)

The genuine `cpu_local.rs::SCHEDULER`, SchedulerAccessGuard and mut/ref entry
points are deleted, together with every consumer and the native guard copy.
Current, spawn, trampoline and cleanup use the SAME independent owners in native
and production builds. The unused unbound process-catalog snapshot/API is deleted.
No renamed lock, copied backend or conditional legacy entry replaces the global.
The remaining `Scheduler` bodies/type and native duplicate banks have now been
physically deleted as one integration batch, not renamed or cfg-hidden. Native
historical witnesses and source/formal bindings now exercise the actual owners;
the complete source-refining joint boundary has now passed the §2.7 deletion
audit. Native, source and mutation success in isolation is not that audit and
never constitutes hardware acceptance. The native namespace fixture contains isolation guards only,
never a copied catalog or execution permission.

The obsolete final Local/Direct/Current validators and SelectedPublishedFrame
(including native unit Ready) are deleted. All actual final qualification now
retains the SAME DispatchFrame/allocation/funding through original admission and
claim. Fair/Direct observation also owns allocation and reader exclusion through
its LAST token check in native as in production. Published pointers, scalar IDs
and queue generations without an original allocation refuse before byte reads.
Historical regressions/mutation IDs now exercise those actual owners, not copies.

The catalog fair-selection family is also physically deleted: ordinary/yield and
class scans, locality/burst/min-granularity, User reservation, overdue System,
generic ordering-hint consumption and duplicate runtime-accounting entry. Native
policy witnesses use independently born resources and the SAME production fair,
age and priority chain; only earlier CPU/class/age/wait inputs are modeled. The
original Current frame/interval settlement publishes its own fair membership,
retaining both actual banks. Independent completed-choice counters follow the
actual physical claim in production; they are policy, never execution custody.
The entire catalog FIFO consumer family and isolated native FIFO banks are now
physically deleted, including spawn/activation/bootstrap, synchronous/Direct/
immediate, hint/latency admission and chain recording. Historical FIFO tests
execute SAME born-resource original frame/funding claims before pop, unknown
prefix retention, actual priority and shared synchronous FIFO/chain-clock state.
No catalog fallback or cfg-hidden native consumer grants a turn. Queue/IRQ inputs
and preparation-bank completion remain native models, not architecture/RSP ACK.
The complete catalog IPC donation/class-recursion/charge family and native
ledger/hint mirrors are physically deleted. Scalar reserve/bind/upsert ledger
entry points are also deleted, not cfg-hidden: original participant/root loans
are the only remaining insertion path. Historical IDs now drive actual owned
edges, exact namespace/worker binding, bounded MAX_TASK capacity, terminal
withdrawal and previous-reply restoration. The independent root-compressed
source permits chains beyond four without recursive dispatch stack growth;
original source revocation still independently refuses reserved nested binds.
The nested billing witness uses three real process/context/domain births and
an original captured interval retained across reply withdrawal, not a late
scalar donor lookup. Its earlier Running selection is a native input, not
architecture/RSP or complete dispatch-interval refinement.
The whole catalog self/peer exec publication/admission/census/quiescence family
and user-thread reservation/commit/cancel family are physically deleted, along
with their native algorithms and reservation bitmap. Original Ticket/Join/MM/
Cohort owners alone authorize these paths. Historical witnesses now exercise
actual born resources: frozen exact-UID census, retired borrower drain, original
pre-storage and complete reservation exclusion, complete affinity replacement,
and wrong-ABI/creator/terminal refusal. Negative descriptor/kernel-pending fault
probes restore on unwind and hold no raw leaf across callbacks; they never grant
execution or substitute for physical reuse or kernel ACK.
The whole catalog kernel/root allocation and native root producer family, mutable
stack allocator/reset and entry-frame construction adapters are now physically
deleted. Original root/kernel producers and their Ticket writers alone construct
images. Historical activation IDs use actual suspended births. The entire catalog
Current wait arm/cancel/commit and generic wake algorithms (including native
copies), and source migration predicate, are deleted too. Wait witnesses retain
actual original physical Current through consumed-frame exclusion, typed arm/
cancel, raced wake, committed-wait atomic refusal, and independently retired or
suspended admission refusal. Original Current capture checks suspended lifecycle
under the SAME Identity writer as its backing loan, not a separate scalar
snapshot, without replacing its physical bank. Transition wake witnesses preserve
physical identity through duplicate mailbox publication, actual drain/transfer
and once-only original frame/funding claim, never clear identity to enable scalar
claim. Earlier Transition/IRQ/RSP arrival remain native inputs, not hardware ACK.
All remaining catalog context/control/lifecycle bodies/type were subsequently
deleted together. Historical identity/hint tests retain original root+sibling
physical bindings. Terminal tests now complete actual kernel effects before
independently refusing runtime Pending and Claimed cleanup, then accept only
exact ACK and external reaper completion; neither MM scalars nor generations
substitute for the original resource seals. Affinity tests isolate the task
qualifier while preserving the process-container subset invariant and verify
every member's complete mask tuple. Caller-CPU IPC witnesses distinguish the
previous CPU with both CPUs admitted. Diagnostic counters and earlier native
CPU/IRQ/queue/frame inputs remain models, not hardware custody. All historical
mutation IDs remain bound to actual owners; whole-type/backend/global deletion
is also source checked. The same two-CPU foreign source-refining protocol now
joins selection/readers/class/context/domain/owner CAS with original MM+UID
Current/Transition banks, donor-metadata replacement, interval settlement,
shared-MM exec census, independently exact peer/source cleanup ACKs, real reaper
and fresh constructor/terminal incarnation. This is one interleaving state space,
not a new disjoint component success; older modes and obligations remain intact.
Its actual native trace completes every user-task kernel effect/ACK and physical
reap through original Executors and Reap::complete, never footer ACK adapters.
Initial user MM backing is owned distinctly from kernel MM before birth; active-MM
exclusion is not bypassed by scalar replacement. Old runtime ABI payload replay
must refuse even after the next incarnation's row is already Claimed.
Full source-refining joint acceptance for this whole deletion includes the
complete validation and requirement audit recorded in the current handoff.
Native privileged effects, roots, initial kernel admission, IRQ/topology input
and incoming RSP arrival remain models; these do not certify hardware execution.

IRQ/cold shared scalar leaves retain a private `irq_leaf::IrqLeaf` guard. It
masks original local IF before the unchanged ranked TrackedSpinLock acquisition,
releases that raw guard before restoring IF and never acquires a CPU dispatch
unit. Failed try, nested acquisition and unwind preserve the original mask;
non-Send custody cannot move an IRQ restoration across CPUs. Native IF effects
are explicit models, not privileged execution. The same owner guards cleanup,
thread work, budgets, donation, queues/mailboxes, policy/FIFO, affinity/wait,
diagnostics, Linux metadata and intervals. Identity/Resources already mask their
bounded writers. Cold ProcessPlacement/domain arbitration likewise masks its
bounded callback, never a nested dispatch owner. No raw guard crosses external
work and no IRQ safety/rank/dependency checker is bypassed. This supplies the
local exclusion assumed by atomic owner leaves in the source-refining models.
Initial affinity publication additionally borrows the original Committing Ticket
through exact published Identity51 then Affinity54, before the Blocked ACK;
unpublished scalar fixtures cannot authorize actual kernel/root/thread births.

Any successful priority arm discharges its exact spawn/activation first-turn
promise through the SAME retained claimed return and priority choice. Completion
withdraws only `{slot, task_id}` under local Policy; FIFO-arm completion cannot
consume/decrement a second place. Unknown or reader-busy fronts still retain
priority. Exact withdrawal shifts only the occupied suffix, never capacity-sized
scratch or a reused incarnation's promise. Shared ordinary byte/handle service
RPC callers likewise arm EndpointReply with their SAME reply handle before the
second poll; a Generic arm is not compatible with the strict typed reply producer.
The two-poll/deadline and original physical/wake/FIFO lifetimes remain unchanged.
Bootstrap's same-state-space earlier-qualified-claim action removes its satisfied
promise while preserving all other FIFO order; unrelated overdue turns do not.

Earlier guarded models remain component history, not source-refining evidence for
the absent global. Post-deletion hardware and performance acceptance remains.
Latest 1-vCPU hardware reaches core-service readiness and initd deferred spawn,
but exposes a remaining cold broker composition: single/batch deferred activation
holds a compat registry raw guard across scheduler activation. Original Current
borrowing correctly refuses that composition. The guard-free original registry
transaction/cleanup boundary is NOT yet established; source-only joint acceptance
did not prove it. Do not bypass that refusal with scalar lifetime relookup or
weaken the original Current/raw-lock gate. Native FIFO leaf measurements are not
a controlled shipping scheduler/IPC benchmark.

#### Strict original return and bounded terminal/reuse join

Resource-owning native turns now execute the SAME architecture/placement/active
interval qualification and BOTH physical/MM and UID bank publications as the
production return. Integer-only protocol fixtures must explicitly construct a
phase-model turn; that projection refuses a successful physical claim. There is
no blanket native exemption for the actual IRQ path. The original claimed
architecture uses one `install_with` state machine: its turn/receipt loans span
all CR3/stack/segment effects and SIMD, and ready follows the LAST effect. Native
witnesses intercept only register/SIMD effects, not admission or phase changes.
Same Current restores SIMD without replaying architectural registers.

The bounded actual producer/FIFO/claim/return join retains outgoing Transition
banks through the real unique RSP callback. An original source terminal request
defers until BOTH banks drain; donation revocation cannot substitute the active
interval's original FundingPin. External/kernel ACKs and the original reset and
resource/MM seals then perform real source reuse. `LifecycleExecutionClaim`
models these shared transitions and preserved component obligations. Earlier CPU
admission, privileged effects and arrival on incoming RSP remain modeled inputs.

The same source-refining join now continues through four actual returns across
independently born process roots: call, reverse reply, terminal source to peer,
and terminal peer to the earlier kernel executor. Both original born banks
leave through actual RSP callbacks before cleanup/reset; a new UID and MM are
born in the same slot while original kernel Current/UID/funding remain retained.
The Blocked outgoing CAS binds its committed frame generation under the SAME
queue leaf, just like Local; later reply must not revive an old frame header.
Process claims require their original activation pin; pure kernel claims use
the original retained physical image, with no fictitious process activation.
Native selection now uses the actual published runnable bitmap, not an all-slot
projection. Explicit scalar policy fixtures remain nonexecution witnesses.
Only teardown of the final earlier kernel executor remains a fixture boundary.
This does NOT certify full §2.7/global deletion/hardware acceptance. No QEMU
permission follows from it.

The actual foreign-CPU/shared-MM join additionally freezes CPU0's original exec
census while a born sibling moves through CPU1's real remote mailbox, claim,
architecture and both return banks. Pending incoming RSP, foreign Current and
outgoing Transition ALL refuse quiescence. The unique CPU1 RSP callback drains
both physical/MM and UID custody before original deferred detach is published.
The SAME non-Copy original kernel executor retains its claim through external
RTC/IPC completion, common actual process detach and exact row ACK. No raw leaf
or dispatch scope spans external work; a fresh masked scope owns only the final
row ACK. Native external completion cannot replace process detach or that ACK.
Only after this consumer does the original source image lease authorize, stage,
publish and finalize self-exec, retire old MM execution state, release census
and reset the sibling. The source later leaves through another complete return
before its own reset. `LifecycleExecutionClaim` preserves the earlier modes and
adds this bounded foreign protocol. Native thread-local CPU identity changes
only the earlier architectural input; it is not admission, claim or bank ACK.
Privileged effects, RSP arrival and earlier kernel executors remain modeled;
this is still not hardware or full guard-deletion evidence.

#### Independent Current terminal entry family

The entire legacy Current fault/exit and single-task termination entry family
is deleted, including its old native process-exit projections. Real APIs use
`current_terminal`; both native and production process entries use the same
original PendingExit/process-row worker. Original masked CPU scope borrows the
parked physical allocation/MM and UID permits, plus the unique original CPU
interval, through the LAST terminal recipient/consumer publication. These are
loans of existing non-Copy bank units, not fresh pins admitted after close or
scalar generations standing in for lifetime. Interval consumption/replacement
refuses until the effect loan releases; normal once-only charge retains its
original FundingPin. Target BindingPin transfers to the original Producer before
borrower drain; last-task process mark uses its exact handle. Group mark precedes
durable worker publication, with no census/raw leaf over process-table work.
Native earlier CPU admission and external ACK remain modeled. Bounded Current
borrow/terminal/settlement refinement is not the full §2.7 joint proof or hardware
RSP/MM acceptance; remaining legacy bodies/type still require deletion.

#### Production owners

| Responsibility | Authoritative owner / rule |
| --- | --- |
| Run intent, block/arm/reason, Local/Remote/Running custody | `runqueue` owner word and generation-bound queue/mailbox transitions; hints never create custody |
| Per-CPU ordering, bursts, first-turn and latency FIFOs | `dispatch_policy` CPU locks, not production Scheduler arrays |
| Lifecycle and task identity admission/writer custody | per-slot `current_identity::IdentityWriter` (class 51) owns four lifecycle gates and serializes identity publication; atomic readers require complete even identity; unavailable/odd evidence is unknown |
| Task/process affinity | stable published intersection, Online and root-CPU restriction; migration-pending evidence is unknown |
| Context/domain budget admission, preparation and charge | exact task/donor stamp, Context→Domain runtime cells and policy/CPU checks; changed donation tokens invalidate observations |
| Nonexecuting Local / Direct selected frame | distinct generation-bound `frame_publication` tokens; saved RSP, epoch and custody rechecked before the same-generation claim |
| Executing IRQ frame | exact Current CPU plus Running generation (same or immediately prior publication); fresh RSP uses published geometry and live canary |
| Retained-current final frame and fallback | fresh Local token with same-CPU Current custody; qualify lifecycle/affinity/budget/frame, prepare budget, claim the same generation |
| Terminal CPU idle fallback | fixed unbudgeted CPU-idle task; exact published Local saved frame and owner claim, never an arbitrary peer exemption |
| External retirement cleanup | independent per-slot Pending→Claimed→Complete cells; terminal producer publishes last, exact full-payload ACK precedes reset/reuse |
| Root/fork final publication | actual Ticket + checked full-process RootJoin; physical pending exclusion spans Slot ACK→membership ACK; registered abandonment retains poison/pin |
| Retired metadata and external detachment | exact Class50 reset + Identity51/Resources53 Reuse grant; all refs/funding drain before effects, both seals survive kernel-Executor-owned external completion |

Production ordinary fair, overdue-System and due-User reservation decisions use
complete CPU-local evidence in profile-on and profile-off builds. A stable
negative lifecycle/affinity or exact exhausted budget may omit only that peer.
Missing identity, odd queue/class/affinity publication, stale budget stamp,
changed donor token or unavailable frame invalidate the observation. Fresh
bounded retries may resolve it; exhaustion uses independently qualified
current/fixed-idle custody, never a catalog peer. Unresolved earlier policy
cannot release later hint/fair arms; only complete no-peer evidence can.

Queued Generic hints are ordering-only. On consumption, Local hints bind a
fresh exact generation; RemoteQueued retains mailbox custody. Rejected Direct
records materialize only their consumed generation. Activation/spawn first
turns carry `{slot, task_id}` and bind the exact Local generation; recycled
identity loses priority, not custody. Non-FIFO User hints additionally require
a stable published strict-System veto; an unknown System peer vetoes the hint.
Latency FIFOs admit stable User class and exact task identity, carry both across
queue lifetime, recheck on consumption, and replace same-slot records inside
one CPU-policy lock transaction.

Missing/unbound/foreign selected frames in ordinary and committed-fast IPC
are stale, never catalog frame authority. A stale token abandons its choice
before budget preparation; a still-current invalid frame uses corruption
retirement. A refused budget or changed owner claim never executes the peer.
Preparation matures due refills without charging unexecuted runtime. Dispatch
IDs, next-task presence, interval-accounting presence and fallback timestamps
no longer gain authority from catalog `starts`/`contexts`. Private host
Scheduler fixtures retain legacy catalog state; that is not production evidence.

#### Execution funding and stack allocation custody

`execution_interval.rs` owns one class-52 CPU bank per executing turn. Opening
consumes the exact context/domain, funding task and reply captured by budget
preparation. The prepared grant pins its banks, rechecks current task/donor
publication and passes through claim into the executing turn; turn opening
never rebinds it to a replacement donor. Pin acquisition also refuses credit
that another CPU drained after preparation. Settlement consumes once before
fair-share bookkeeping, even following reply return/cancellation. Non-Copy funding pins prevent context
release and domain replacement. Reclaim closes the context against new pins;
existing intervals still charge, then their last release notifies the reaper.
A settled receipt makes duplicate same-turn accounting a no-op. Only unbudgeted
boot/AP idle may begin outside a scheduler-selected grant. Bootstrap work is
not yet Idle: initial admission explicitly opens its RTC-based bank, as does
secondary admission without importing the constructing CPU's local clock. Local-clock subtraction
is same-CPU only, with RTC fallback; minimum fair charge does not invent budget
consumption. This is kernel custody, not new userspace budget policy.

Production Scheduler's stack-allocation array is deleted. Class-53 resource
cells own each Vec and exact task/geometry binding; construction allocates
outside the leaf, and final detachment returns storage for destruction outside
owner locks. Pins precede published Local/Direct/retained-current frame byte
validation and prevent reset/release/rebinding. Production Ready carries the
non-Copy pin through final claim into CPU-private prepared/Current/Outgoing
banks in `stack_resources/cpu_leases.rs`. Initial CPU publication owns its
allocation before Active; changed publication parks the outgoing reference
before exposing incoming Current. Assembly clears Transition after installing
RSP, then releases only the outgoing reference. The parked references use
atomic consume-once custody, not locks, allocation or reference resurrection;
same-task continuation never loses its final reference.

`cpu_local/dispatch_turn.rs` now owns unique CPU-local return/scratch custody,
not the global Scheduler diagnostic owner word. A non-Send/non-Sync token
captures exact CPU/from; atomic Open admission rejects overlap. Sealing prevents
second publication; changed return waits for the incoming-RSP assembly edge,
which clears Transition, consumes Outgoing and then releases the local gate.
Duplicate commit cannot release an Open producer. Abandoned/partial publication
poisons the gate rather than pretending architectural rollback is safe. The
CPU token now receives independent IRQ/preemption custody **before** waiting
for the directory raw guard. `lockdep/preemption/dispatch.rs` owns a unique
non-lock CPU unit; raw depth remains held+pending, while effective depth adds
this unit. Entry briefly masks IRQs before deriving CPU identity and publishing
the unit; scheduling IRQs then defer, but caller-enabled TLB/device IPIs remain
serviceable during the bounded directory wait. After acquisition the unit masks
IRQs continuously for protected dispatch/hardware work. A non-Copy completion
keeps Publishing/IRQ custody through raw unlock and deferred IPI emission. An
unchanged return then releases its unit and restores caller IF; a changed return
parks the same unit without restoring outgoing IF. The exact Awaiting-RSP→
Committing consumer releases it on incoming RSP before reopening. Duplicate
commit cannot spend another producer; poison retains IRQ masking rather than
pretending to roll back published architecture. Private scratch resolves only the executing CPU's Open
cell; `SCHEDULER_OWNER_CPU` remains timeout/profile telemetry, not permission.
The return owner checks admission, original Current and remote exclusion before
publication. A changed return also requires the exact slot/UID/Running-generation
executing interval on that CPU, with stable Running custody. Every completed
return cancels unused prepared funding outside the interval leaf, retaining
active funding and foreign CPU grants; final closing-funder release notifies
reaping. Native turn/cancellation witnesses refine these source interlocks;
`CpuDispatchPreemption` and five core/four turn-scope native witnesses refine
the smaller directory-wait/unlock/park/RSP-release boundary, not physical
IRQ/APIC execution, outer raw locks or the complete joint claim/grant/MM/register
transaction. MM/CR3/TLB and cold cross-owner directory
transactions remain guarded; this is not authorization for guard deletion.

MM architecture preparation additionally retains a private non-Copy,
non-migrating `ExecutionRootLease`, acquired from exact Current/Prepared context
custody. The user-root reference is cloned under resource admission through the
MM owner's open/close CAS, then retained outside the leaf through CR3/TSS/TLS
installation or the same-task fast return. Kernel/bootstrap roots still require
the external architecture owner; absent user-process custody is not permission.
CPU turn custody is required for production architecture entry. This is physical
root retention and exact credential capture, **not independent MM close versus
complete activation/publication arbitration**: image/sibling quiescence and the
cold directory still provide that transaction. Captured sealed roots retain
backing but cannot grant a fresh architecture lease. The real context/MM-bank
witness independently checks the lease pin survives context release.


The same class-53 cell now owns the immutable task ID and kernel entry descriptor
(`stack_resources/task_start.rs`); production Scheduler's `starts` array is
removed. Snapshot readers copy the descriptor under the leaf, never hold its
lock across entry execution. The production entry trampoline no longer acquires
Scheduler: an IRQ-excluded lookup requires allocation-bank Current (not merely
Prepared/Outgoing), exact complete published identity and live lifecycle. Missing
or stale identity cannot recover catalog permission. Frame binding and CPU
admission require the same owned ID. Closing blocks descriptor publication; selected/Current/Outgoing pins
block clearing it. Same-ID exec may replace only the entry, not identity or
reference counts. Slot reset closes, waits for all references, clears the old
descriptor and detaches backing before a different ID can bind. Monotonic ID
allocation and complete process/exec transactions remain directory-owned.
The class-53 owner also stores complete context metadata
(`stack_resources/context.rs`); production Scheduler's `contexts` array is
removed. Readers receive copied scalar records, never mutable directory
references. Context installation checks the encoded slot and empty descriptor;
start publication checks its exact funding handle's task ID. Clearing waits for
zero allocation/descriptor references and closing. Only narrow same-ID exec
patches (ABI/session/stack) and exact-task/exact-handle terminal detachment are
mutable; they cannot replay unrelated fields from an old snapshot. Process
membership reference transfer still belongs to the outer terminal/exec
transaction, not to a copied handle or metadata record.

Physical process-object custody now uses a static generation/count/closing
atomic gate (`process_table/pin_owner.rs`). Directory-independent pins must be
acquired before dereferencing the published state-lock pointer. Reap closes the
same gate before observing zero and detaching the box; new task attachment
cannot resurrect a closed object. Last release requests a consume-before-scan
rescan, without locks/allocation or process-table entry. Context installation
captures exact process generation/PID custody, and terminal handle detachment
leaves that pin in the resource cell until final context clearing. Consequently
selected/Current/Outgoing descriptor references also retain the state-lock box.
Syscall `ProcessRef` and direct own-state callbacks hold independent atomic
pins: task membership, cached labels or directory guards are not their lifetime
proof. This does **not** retain an old exec address space; generation/visibility
checks and staged old-state/CR3/TLB transactions remain mandatory.

Linux thread-state storage is also a static class-39 owner, not production
Scheduler fields. Binding uses joined resource Current and exact TID; static
pointer lifetime and post-lock TID rejection prevent reuse aliasing. Mutations
and pending-signal hint synchronization share the leaf through the bounded
IRQ-excluded callback; raises and lowers no longer rely on Scheduler exclusion.
Userspace signal policy and existing exec/retirement transactions are unchanged.

Live SIMD image accesses now have an independent per-slot exclusive atomic
RAII guard before any raw byte borrowing, plus an exact task-ID binding (zero
is valid root identity). Save requires this CPU's Current allocation bank;
restore may also use its Prepared bank, never Outgoing. Construction requires
Dormant/unbound/zero-reference custody. Replacement requires exactly one own
Current reference, or no banks/references plus terminal closing or peer-exec
quiescence. Any Prepared/Outgoing/foreign Current bank refuses replacement.
Terminal release clears the binding before UID reuse; same-ID exec preserves it.
Peer exec keeps its no-dispatch gate through SIMD, descriptor/identity and Linux
TLS/state publication. The hardware format and userspace policy are unchanged.
These checks remove raw SIMD memory's reliance on Scheduler exclusion;
admission/exec transactions and complete architectural CPU publication still
retain their existing guard.

Frame bytes now also have independent exclusive writer custody in
`stack_resources/frame_access.rs`. Unpublished construction requires no start,
binding or references; exec requires an exact slot/UID, quiesced lifecycle,
no Prepared/Outgoing/foreign Current banks and either no references or exactly
its own IRQ-stabilized Current reference. The resource leaf atomically excludes
all new frame pins before exposing actual owned allocation geometry. Byte
initialization runs outside the leaf with IRQs excluded. Closing cannot detach
storage/context/start while a writer exists; final writer release re-notifies
the reaper. Alternate/boot backing is not granted by this owned-primary lease.
Peer exec acquires the lease before sibling/image side effects and retains it
through the full frame/identity/TLS/MM publication and dispatch-gate release.
It revokes old frame epochs before the first byte mutation; pinning rechecks
the epoch after acquiring allocation custody. Earlier tokens therefore cannot
borrow a replaced image, and a newly published token still cannot read an
incomplete transaction. Trusted fixed-ABI writes live in `execution_image.rs`;
only constructor/exec lease-qualified paths reach them in production. Physical
byte custody does not replace old-MM/CR3/TLB or cross-owner transaction custody.
Self exec acquires the same writer before its scheduler image side effects,
using exact sole IRQ-stabilized Current instead of a reschedulable self-quiescence
barrier. Prepared/Outgoing/foreign Current and extra readers refuse; the lease
spans descriptor/SIMD/identity/Linux/TLS publication and CR3/FS installation.
`scheduler/exec_transaction.rs` now extends image custody across process-state
staging and final visibility, not just the directory image mutation. The
coordinator in `current/exec.rs` acquires exact target/UID/process-generation
custody before replacing process state. Refusal cannot stage the new MM;
unused callback/MM inputs are destroyed after IRQ restoration. No resource
leaf spans ProcessState/ProcessTable, directory or hardware work. Self and peer
paths keep the lease through finalization; by-PID Current replacement uses the
self path instead of rewriting a live stack or acknowledging uninstalled CR3.
A non-Copy, once-issued publication receipt binds the full exec reservation
(process generation, expected/next MM generation and transaction nonce) and
new root. Issuance checks the target's published root and, for Current in the
boot image, real CR3. Finalization consumes receipt plus staged old state,
checks the exact live transaction before visibility, then allows old-state
release outside image/IRQ custody. The HAL inactive-root/global-shootdown
retirement owner remains mandatory. Existing sibling quiescence, directory
admission/claim exclusion and complete MM/architecture transactions have not
been replaced: the receipt alone is not an MM execution pin or guard-removal proof.

Physical MM execution lifetime now has its own owner in
`kernel/mm/src/memory/address_space/execution_lifetime.rs`: a non-Copy root pin,
checked atomic references and irreversible close. Process publication and each
resource context retain exact process-generation/MM-generation/root custody.
The context cannot release its pin while selection/Prepared/Current/Outgoing
stack custody remains. Closing blocks new resource claims and root activation,
not captured-reference lifetime. Architecture returns require the incoming
Current/Prepared bank and an open exact root; absent user process custody is
not a kernel-root exception. Exec binds the staged next-generation pin to its
image lease and transfers it only after root/Current CR3 publication checks.
Existing sibling/image quiescence still arbitrates that transition.

MM activation now has a separate checked atomic admission gate. A non-Copy
`ActivationPin` retains a physical ref; grant admission and cold close contend
on the same gate. `seal_execution() -> bool` refuses without any partial close
while a local or remote grant exists. Idempotent close completes both admission
and physical-reference close publications. Physical owner Drop may revoke
admission but cannot free captured backing; it remains hot/atomic-only.
Both exact Local/Direct Running CAS paths capture process/MM-generation/root
and immutable slot/UID custody under the reader/claim resource leaf, bind the
successful owner snapshot, then transfer the grant outside leaves to the unique
Open CPU turn. Failed CAS drops its provisional grant outside resource/queue
leaves and preserves owner/membership. Architecture-only Current/Prepared
returns also capture a grant; dropping the lexical physical root lease cannot
release CPU publication custody. Exact slot/UID/MM/owner currency is rechecked
before return publication. Exec captures the staged next-MM activation before
its direct CR3/FS work; after exact root/CR3 receipt checks and resource MM
replacement, Current exec transfers that same grant to the CPU turn. Peer exec
retains it in the image coordinator through finalization, not a foreign CPU
bank. A staged nonce alone is not activation custody; the native witness tests
the MM gate independently before consuming the receipt. Unchanged completion
releases after raw unlock;
changed completion retains the grant through incoming RSP/Transition/Outgoing
ACK and releases before reopening. Poison quarantines successful grants: no
unproved cancellation/ACK can authorize MM close. Only isolated host fixture
recovery releases poison custody. Exec staging refuses before replacing state
or publishing a generation; existing callers cancel the exact reservation.
Five MM atomic and six real resource/process/turn witnesses plus
`MmActivationClose` refine this admission/claim/architecture/RSP boundary.
The model includes one remote grant and exact owner republish, but excludes
weak-memory, physical refs/retirement, hardware/TLB, funding and complete
lifecycle/remote-quiescence arbitration. Admission does not authorize a stopped,
retired or foreign task and does not remove the genuine cold directory.

Old exec state transfers its original MM owner into a cold retirement payload,
retaining VMA/shared-backing/handle metadata as well as PTEs/data frames until
all captured execution refs drain. There is no payload→owner reference cycle.
Hot Drop only closes/releases/enqueues preallocated storage, without owner
locks, allocation or HAL work. One bounded cold consumer validates zero refs
before taking destructive ownership; IRQ-disabled calls leave work pending.
The unchanged inactive-root/global-shootdown and frame-ledger finalizer runs
before payload destruction. Six native atomic-owner/queue witnesses and two
real process/resource/MM-bank witnesses cover this transfer; modeled host roots
do not execute CR3/TLB. Existing bounded models do not refine this queue,
payload, atomic references or the complete claim/architecture transaction.
This is not permission to remove the directory or HAL retirement owner.
Linux `sched/mm.h` and `active_mm.rst` motivate the distinction between process
membership and executing MM custody, not vendor equivalence or locking proof.

Production diagnostics and return-mode inspection now copy POD header data,
never escape a static storage reference. `frame_observation.rs` joins exact
published RSP/epoch, resource UID/pin/bounds and stable run ownership. Executing
inspection requires this CPU's Current or incoming Prepared bank; foreign
Current refuses. Unknown return mode fails stop, never suppresses architectural
restoration; unavailable telemetry is not a zero-frame classification.
Saved-header byte readers now have independent resource-leaf admission in
`stack_resources/frame_readers.rs`. A non-Copy, CPU-local guard borrows an
allocation pin, registers under the same class-53 leaf and spans validation or
POD copying plus final token recheck. Both Local and Direct→Running CAS paths
execute under that same leaf with no admitted reader/writer; a pre-CAS count
check followed by leaf release would be insufficient. Refusal preserves the
exact owner, queue membership and handoff generation. No leaf spans the byte
read, allocation, queue acquisition or hardware work; the local claim acquires
RunQueue before the resource leaf and publishes queue removal only after CAS.
Image writers explicitly refuse readers. Checked registration/release and
unwind preserve allocation custody; banks remain a separate lifetime owner.
Running/retained-Current reads require own Current/Prepared and continuous
own-CPU IRQ/preemption exclusion; boot admission checks caller IRQ state before
entering the leaf, not the temporarily disabled state inside it.

Non-executing primary diagnostics therefore no longer use directory claim
exclusion for byte stability. Borrowed alternate/boot backing still requires its
external lifetime owner. Six native real-resource/real-CAS tests and the bounded
`FrameReaderClaim` model bind reader/claim leaf exclusion and exact owner-word
revalidation, not running IRQ reads, MM backing, hardware restoration, complete
cold arbitration or guard deletion. Independent CPU preemption custody is now
connected, but the complete grant/claim/architecture transaction is not sealed.
Pins alone do not exclude hardware execution: these copies do not prove
independent shared byte-reader/claim arbitration or global-guard removal.

Scalar current ABI fallback, stack-layout and console-session queries no longer
acquire Scheduler. Current process/MM-binding fallbacks, process-handle queries
and Linux-state/binding lookup now use these owners as well. IRQ exclusion plus allocation-bank Current and one joined
context/start/funding binding authorize only copied metadata. Prepared/Outgoing,
missing records and mixed funding identities refuse. These scalar queries do
not grant dispatch or retain MM/address-space backing; retained process/MM
access still uses its existing lifetime owners and cross-owner transactions.

In-place exec refuses a thread whose kernel task ID differs from its process ID
before sibling retirement or privileged image work: it cannot silently alias
an existing task identity. Userspace still owns exec/group-transition policy.

Closing cannot be undone by metadata refresh. Failed pin acquisition is stale/unavailable, not corruption.
Pin creation is lazy: a refused acquisition must never manufacture a destructor
that releases another reader's reference. Last-pin release is lock-free.
Borrowed alternate/boot stacks retain their existing external lifetime owner.

Native real-cell witnesses cover interval consumption, captured reply, clocks,
funding close/reuse and concurrent allocation pins. These new lifecycles are
not proved by the existing bounded formal slice. Native real-bank witnesses cover claim/publication gaps, both allocations
through assembly commit, duplicate commit, same-task refresh, rejected/foreign
selection and remote close. Host frame fixtures still omit production pins.
Slot reset/reaper SIMD teardown wait for these allocation references; borrowed
alternate/boot backing, old-MM/CR3/TLB lifetime and complete
cross-owner frame/admission/exec/reaper transactions still require their existing
owners and guard. Six additional native descriptor witnesses cover pinned reset, same-ID entry
replacement, closing, UID reuse before frame binding, concurrent readers and
running-entry refusal for missing/foreign identity or non-Current custody;
one exec witness covers refusal before a live thread can alias its process ID.
The existing side-effect refusal witness uses a valid leader identity so that
the new identity gate cannot mask retirement-token regressions. No bounded
model currently refines the parked-reference bank or task-start owner. Eight
native context-owner witnesses additionally cover pinned teardown, exact exec
and detach patches, foreign slot/funding identity, concurrent copied reads and
joined Current metadata; the bounded models do not refine this new context
storage/lifetime either. New native process-gate, descriptor-to-process and
Linux-state witnesses cover physical reap/reuse/notification and exact TID/hint
serialization; existing bounded models still exclude these new atomic gates
and do not prove old-MM exec or complete guard-free dispatch. Six native SIMD
witnesses additionally cover identity/terminal reuse, exclusive token and
unwind/concurrent access, Current-vs-Prepared permission and actual CPU-bank
replacement custody. Six further frame-writer witnesses cover unpublished and
exec admission, exact lease credentials, readers, detach/reentry/unwind,
concurrent close, final-release notification and epoch revocation/publication.
The bounded models do not refine either new image guard.

#### Independent identity/lifecycle writer

Production `retired`, `start_suspended`, `job_stopped` and
`exec_target_quiesced` Scheduler arrays are deleted; only isolated host fixtures
retain them. All 73 read sites use the owner adapters. Each class-51 cell owns
identity plus lifecycle and commits its atomic read-only publication in one
transaction with local IRQ exclusion. Metadata writers cannot supply flags,
so a metadata refresh cannot overwrite a newer stop/suspend/quiesce intent.
Absent partial-initialization metadata preserves pending intent; explicit
admission/reset clears it. Rebinding a live task ID without clearing fails
stop; exact live commands reject prior identities after reuse. No allocation,
user copy, external work or lower-ranked owner runs under this leaf lock.
Native cell tests exercise metadata/flag races, absent metadata and reuse;
they do not prove complete production admission/exec/switch equivalence.
The directory still constructs process/budget/frame metadata and assembles
cross-owner activation, affinity/class, exec and terminal transactions.

Lifecycle admission now holds the class-51 writer across exact UID/complete
user-binding qualification, the class-53 resource/MM admission leaf and the
Running owner CAS. Local claims hold RunQueue40→Identity51→Resources53;
Direct claims use Identity51→Resources53. No budget, allocation, process lock,
queue acquisition or hardware work enters those leaves. Refusal preserves the
exact owner/membership and releases provisional references outside the leaves.
A non-Copy execution permit retains the bound UID through own-CPU
Prepared/Current/Outgoing banks. Desired stop/suspend/quiesce/retire closes new
admission, not already captured CPU custody; it is not a completed remote
quiescence ACK. Hidden metadata cannot erase the retained binding, and clear,
descriptor/context teardown, reaping and peer image mutation require drained
execution references. Sole exact Current image replacement remains allowed.
Architecture return additionally requires Current/Prepared execution custody.
Remote exec/terminal quiescence also waits for captured references even before
Current/Transition records a CPU; a known run-owner CPU is only a reschedule
hint, never permission or an ACK. Self exec remains its explicit sole-Current
exception. Thus a claim-to-Prepared gap cannot masquerade as a drained peer.

Assembly first admits a unique Awaiting-RSP→Committing consumer, then clears
Transition and releases outgoing stack/identity banks, then releases MM
activation/preemption and reopens the CPU. Refused/duplicate ACK cannot touch
those banks or a live producer's transition. Abandoned admitted ACK poisons
without fabricating completion. CPU-local atomic reference release allocates
nothing and the last release re-notifies the reaper. Native fixtures explicitly
isolate/release bank holders; queue-only fixtures do not grant user execution.
`lifecycle-execution-claim/LifecycleExecutionClaim` refines a bounded single-slot
intent/claim/identity-bank transaction (259 generated/89 distinct/depth13).
Twelve lifecycle and three admitted-ACK native witnesses cover independent
qualifiers, real Local/Direct refusal, writer concurrency, saturation/unwind,
UID hiding/reuse and early peer mutation. The model excludes same-task refresh,
weak-memory refinement, physical stack/MM and hardware, full affinity/class/
funding and remaining cold transaction arbitration. The global directory guard
therefore remains genuine and is not deleted by this boundary.

#### Prepublication slot custody and domain descriptors

Production `Scheduler.thread_slot_reserved` and `scheduling_domains` arrays are
removed; isolated fixtures alone retain them. Class-53 resource cells own a
monotonic slot reservation generation and exact UID through
Reserved→Committing/Canceling→complete. A non-Copy ticket survives sleepable
thread preparation. Ordinary storage/context/descriptor installation and
storage detachment reject a reserved slot; ticket operations compare the full
UID/generation/phase. Private process metadata has read-only accessors, not
mutable public reservation credentials. Cancellation retains occupancy across
process detachment and allocation destruction; its final ACK requires no
context, descriptor, binding, storage, writer or pins. Commit ACK requires exact
context/start/binding plus Blocked admission and published UID. Dropped or
partially committed tickets quarantine the slot as Poisoned; they cannot
silently roll back, reopen, or poison a replacement generation. There is no
production poison-reset path. No leaf crosses allocation or process work.

Domain policy/generation descriptors are copied from the class-49 budget cell,
not a second production catalog. Missing budget in a live cell fails stop;
copied metadata neither grants budget nor retains a domain generation. The cold
directory still serializes cross-domain utilization, live-member/replacement
checks and normal slot allocation, and supplies process/MM/activation conflict
arbitration. Ticket storage pins and domain copies do not own old MM execution
or finish the grant/claim/architecture transaction. Nine real-resource ticket
and two domain-cell native witnesses cover this slice; existing bounded models
do not refine these new phases or establish guard-free production equivalence.

#### Independent external-cleanup owner

Production Scheduler has no cleanup, kernel-side-effect, deferred-reason,
retire-reason arrays or mutable reaper-work flag. One
`SchedulerRetirementCleanup` lock per slot owns deferred producer custody and
both Pending→Claimed→Complete transactions. The terminal directory producer publishes
its immutable task/process/Linux payload as its final operation, after lifecycle,
donation and side-effect metadata. Claim/ACK consumers take no Scheduler guard
and release the cell before user copy/faults, IPC, waiter or other external work.
ACK compares the full payload and exact claimed task. Altered, unclaimed,
repeated and prior-task ACKs do nothing. Kernel revocation uses a private
non-Copy generation-bound claim, held until timer/IPC/donation/process-detach
and wake work returns; kernel ACK is its final operation. Exec freezes its
process-detach payload and context identity before atomically publishing both
tokens. Taking deferred intent retains producer custody, not permission to
reap. Either pending/claimed transaction or a deferred/in-progress producer
prohibits reset before runtime/slot identity teardown. Reaper work is consumed
by atomic swap before scanning, never a final false store that loses a
concurrent producer/ACK. Lock order is directory→cell, never the reverse;
no allocation, blocking or external work occurs under the cell lock. Native
cell tests cover the new kernel/producer phases; the existing bounded formal
cleanup model does not prove them. Monotonic task IDs remain the allocator's
responsibility. The guard still owns terminal metadata assembly and enclosing
cold execution/process transactions; retired reaper detachment now uses the
independent exact terminal/resource owner below, not directory custody.

External kernel cleanup now borrows a **kernel-only logical resume loan**,
separate from its backing `BindingPin`. Actual admitted Current captures under
Terminal50→Identity51→Resources53 before any external effect claim. Complete
kernel metadata, physical context/start/full UID, live gates, no constructor,
exec/cohort/image writer or closing resource are required; UID0 is valid and
housekeeping is a dynamic kernel task, never a root-slot assumption. A scope
retains backing and prohibits terminal admission, every stop gate, metadata
withdrawal/role change and qualified cohort sealing, **not dispatch**. Its
non-Send/non-Sync guard survives kernel waits and the borrowed `KernelClaim`
cannot outlive it. Assert exact Current before effects and before actual ACK.
User callers carry only a Copy fence of Pending/Claimed generations; a retired
waiter may disappear without orphaning the row. Exact kernel effect ACK advances
a persistent completion watermark; reset/identical reuse/absence cannot invent
completion. No leaf spans callbacks, process locks, IPC or destruction.

A stop arriving during this loan is **not discarded**: the original checked
Class50 producer/reason/detach tuple transfers into WaitingExecutor custody,
without setting a stop gate that would prevent the cleanup kernel from resuming.
Only already admitted nested scopes may finish; new scopes refuse. Waiting
admission qualifies the exact physical kernel even if the last scope released
between terminal refusal and publication. Otherwise that gap would lose a stop.
Claim joins actual terminal binding under Class50 before nonce/effects; untouched
claim Drop returns WaitingExecutor, and effects abandonment still poisons.
Non-yielding deferred terminal work runs before external executor admission,
including on user callers, so a stopped housekeeping worker need not reacquire
its own forbidden fresh loan to publish its stop. Guarded terminal effects remain
an independent future cut. Retired detachment now joins actual per-slot terminal
reset and physical resource reuse seals, retaining both through kernel-owned
external completion. Native
resource/identity/queue and actual ACK witnesses plus implementation mutants
cover these laws, **not complete reschedulable user-transaction, hardware, or
§2.7 guard-deletion refinement**. Existing backing models remain component
projections; a naked user backing pin is never resume authority.

#### Independent retired detachment through final external ACK

`scheduler/reaper.rs` replaces production directory scanning/detachment with a
non-Copy grant joining actual full UID desired stop, Retired run owner, all-CPU
execution quiescence, terminal dual-consumer ACK, exact Identity51/Resources53
reuse custody and irreversible funding closure. Retired resource capture checks
stop in the same identity turn as the resource gate; unpublished constructor
rollback remains a distinct permission. Closing is not a boolean claimant.
Current user-root exclusion, exec/cohort/frame/physical backing and constructor
custody still independently refuse. Untouched terminal capture rolls back on a
failed join; existing funded intervals finish their exact charge before release.

The actual grant owns direct donation, runqueue, context/process/MM reference,
Linux state, all-CPU policy hint, lifecycle, SIMD, descriptor, identity and history
withdrawal. No directory/global replacement mutex owns those effects. Detached
metadata, even after all fields become empty, is not vacancy: both existing seals
and the original hidden binding survive stack/process external completion. Only
its return permits exact terminal ACK and resource/identity reopen. Borrowed
kernel Executor asserts original Current before effects and after return; user
callers cannot acquire the external loan. Abandonment retains terminal seal and
poisoned resource occupancy; native recovery hooks are fixture-only, never an
external ACK or production retry/reopen.

Ten native witnesses execute the shared actual grant/detach/completion law,
including separate lifecycle versus frame backing, pending terminal ACK, exact
funding charge through close, disjoint grants, physical all-CPU hint removal and
abandoned empty-metadata quarantine. Host fixtures publish real kernel SIMD and
stack geometry; they do not prove physical user MM/TLB, CPU assembly, enclosing
constructor rollback or the complete joint §2.7 refinement. The genuine global
guard remains for other cold producers and dispatch, not retired reaper custody.

Affinity replacement now has an independent exact-UID admission owner in
`runqueue/affinity_payload.rs`: the class-54 leaf serializes mask/binding
replacement with the actual Running CAS, not merely the candidate snapshot.
Local order is RunQueue40→Identity51→Resources53→Affinity54; Direct omits the
queue leaf. Resource descriptor/funding/MM qualification precedes affinity;
only the exact atomic owner CAS enters Affinity54. Provisional MM/identity
custody on affinity refusal is destroyed after all leaves. Failure preserves
Local membership/generation and Direct custody. Affinity replacement captures
the descriptor outside the higher-ranked leaf; a live UID cannot rebind without
explicit drained reset. Root UID0 remains valid. Bootstrap root and generic
kernel slots now explicitly initialize their affinity binding before admission.

Both masks and CPU bounds are independently checked. Pending migration is not
permission to reuse the excluded CPU, but also is **not** a veto of an included
destination claim. A non-Copy `PlacementGrant` captures exact UID/CPU/Running
owner and checked, non-wrapping affinity publication epoch under that leaf.
Architecture uses captured permission, not a later desired mask. CPU turn
publication checks exact custody; unchanged return ACK follows directory unlock,
changed return ACK follows admitted incoming RSP. Only matching UID/epoch clears
migration; an old ACK never erases a newer request. Installed Current permission
survives that ACK until the next admitted hardware/RSP replacement. A same-Current
IRQ/image scope can copy the exact installed owner/UID/epoch for architecture
restoration; it cannot authorize a fresh Running claim under excluded desired masks.
Duplicate same-owner capture retains the original epoch; poison retains custody
without a fabricated ACK.
Self exec captures placement before staging/image effects and transfers it with
the exact publication receipt, even if desired affinity changes meanwhile.
A mask update does not retroactively consume captured execution references.
Reset requires zero execution and no cold exec intent, and clears UID authority:
unrestricted reset telemetry is not permission for the former UID. Concurrent
writers share the same leaf; coherent passive snapshots are observations only.
Production admission never uses the explicitly unbound native fixture exception.
Native queue witnesses read the actual published bitmap, not the test-only
all-slots candidate enumeration.

Cold multi-slot publication now joins an independent `current_identity/cohort`
owner. Each exact UID is pinned under Identity51→Resources53 before any effects;
active image writers/closing/foreign allocation refuse. These non-Copy binding
refs retain teardown/reuse custody, **not CPU execution permission**. Exec intent
admission and unowned suspension/mask commands cannot spend a sealed cohort.
Activation and affinity credentials are operation-qualified. All desired masks,
lifecycle and queue effects precede one checked-epoch Release; actual Local/Direct
CAS and initial execution admission acquire that release under Identity51.
Per-slot unseal may lag without exposing a runnable prefix. Refusal before effects
unseals only exact members; partial-effect abandonment poisons and retains UID
pins. Desired terminal stops remain terminal, never reawakened by activation.
Six native witnesses/ten implementation mutants cover these source boundaries;
they do not extend the existing bounded models into weak-memory/group-membership
or hardware refinement.

`multitask/process_placement` now owns bounded exact process-generation/slot/UID
membership and the committed process mask, not a Scheduler enumeration. All user
ABI roots register; thread publication joins the same ledger, so same-UID exec
ABI changes do not lose membership. Identity51→Resources53 retains the exact
creator binding; ProcessPlacement55 admits a non-Copy creation reader before
first frame effects. Readers retain the inherited committed mask through child
registration and suspended publication; process mask writers refuse readers and
creation refuses a pending writer. A writer physically pins the process object,
then seals its live exact members with Cohort. All per-slot desired mask/queue
effects precede committed-mask publication and Cohort's atomic Release under
one short ProcessPlacement55 leaf. Queries see the old complete mask or the new
complete mask, never a prefix; per-slot unseal and cross-owner drops occur after
the leaf. `current::windows_process_affinity` no longer acquires Scheduler.
Context teardown removes exact membership before dropping physical process/MM
custody; pre-effect refusal rolls back, partial failure poisons instead of
reopening, and transaction nonces never wrap. Binding readers are not execution,
MM activation, or process-exit admission grants.

Seven additional native witnesses/nine registered implementation mutants cover
creation/mask conflicts, committed query release, credential operation, ABI/UID
currency, exhaustion/quarantine and real process-object reap retention. Existing
bounded models remain individually qualified; no new weak-memory or complete
membership/exit/hardware model is claimed.

Constructor admission now distinguishes visible descriptors from committed
publication. Every Reserved/Committing/Canceling/Poisoned ticket denies cold
binding (including activation/creator pins and exec seals), frame observation,
and the actual Local/Direct Running CAS under Resources53. Exec seals join
Identity51→Resources53 through publication; reservation also refuses retained
binding refs or an existing exec seal before advancing its nonce. Constructor
byte writes require a borrowed exact live slot/UID/generation/phase Ticket,
not merely an unpublished frame. Ordinary initializers and exec image writers
cannot enter reserved backing; valid producer initialization remains possible
before descriptor publication, with bytes and writer destruction outside leaves.
The final ACK joins exact UID under Identity51 to full context/descriptor/binding
validation and reservation release under Resources53, after Blocked admission.
Six additional native witnesses/nine new implementation mutants cover each
independent refusal, real Local/Direct custody, producer byte admission and
post-ACK reopening. Native model recoveries never constitute production ACKs.

Directory still owns complete creation/activation/exec/exit arbitration,
domain/utilization and terminal/reaper/reuse transactions. The reservation gate
protects an unfinished target, not complete process-exit or activation-registry
transactions. This transfer does **not** authorize global guard deletion.
The process/system-mask interface was checked against Microsoft's
[GetProcessAffinityMask](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getprocessaffinitymask)
and [SetProcessAffinityMask](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-setprocessaffinitymask);
this preserves the existing bounded single-mask ABI, not Windows processor-group
or physical scheduling equivalence.

Eight native witnesses and nine implementation/three specification mutations
cover independent masks/UID/CPU, writer/claim overlap, writer races/unwind,
producer rejection/reset/reuse, real Local/Direct CAS refusal and retained
execution. `AffinityClaimAdmission`: **3,884 generated/888 distinct/depth14**,
five invariants and explicit stutter; independent Boolean mask cases include
adversarial/incomplete qualifier observations, not valid userspace producer
requests. It excludes weak memory, policy authorization, process-group atomic
updates, post-claim placement changes through CPU return/hardware, and full
directory deletion. The genuine global transaction still protects the latter
boundaries; serializing the CAS alone does not justify removing it. Nine further
native regressions and eleven implementation mutations cover captured return,
stale migration ACK, owner republication/poison, self-exec receipt transfer,
affinity epoch exhaustion and sleepable thread reservation conflicts. They do
not extend the bounded affinity model into a physical hardware proof.

A thread-slot reservation now retains its exact creator slot/UID and physical
old-MM pin across sleepable TEB materialization. Commit acquires MM close admission
before frame/descriptor effects and retains it through suspended publication;
exec cannot seal that root mid-commit. Already closed old-MM reservations cancel
rather than resurrect it. Desired masks are re-read from the exact live creator,
not a recycled/arbitrary group member or value-only old snapshot (mask ABA).
This does not replace process-wide/group activation, exit/reaper or domain-policy
cold-directory arbitration, and does not prove abandoned-stack eventual cleanup.

Cold exec quiesce/cancel/publication now belongs to
`current_identity/exec_intent.rs`, not a directory-owned boolean. A non-Copy
intent binds slot, UID, full process/MM/exec reservation and a checked monotonic
slot generation, retained through identity reset. Identity51 owns
Sealed→Cancelling/Image→Published or Poisoned commands; an atomic per-slot word
only authenticates resource access. Old credentials cannot mutate/cancel a
later transaction. Unqualified gate writers, descriptor/context/identity reset
and allocation detach cannot spend active intent. Image access independently
joins mode, identity/context/start, owned credential and exclusive execution/
allocation banks before any effect; desired quiescence remains distinct from
captured references or remote hardware completion.

A self intent remains dispatchable while the sibling barrier reschedules; only
its protected image scope closes self admission. Completion requires the exact
process-finalization credential and no frame writer, and runs after FrameWriter
Drop **before caller IRQ restoration**, so self cannot be stranded by its own
gate. Cancellation closes before process-owner arbitration while a standalone
CPU preemption unit prevents rescheduling and preserves caller-enabled IPIs;
only its exact non-Copy process-cancellation receipt reopens. Pending exit keeps
the gate closed and invokes existing terminal retirement/reschedule work.
Abandoned intent/image quarantines the generation; publication cannot roll back.
No Identity/Resource leaf spans process ownership, hardware, allocation or
external work. The genuine directory still enumerates/quiesces siblings and
coordinates descriptor/hardware installation, affinity/class/admission and
terminal/reaper transactions; this is not guard deletion or userspace policy
migration back to ring0.

Twelve primitive/resource/process witnesses include genuine staged MM/receipt
finalization, independent teardown/writer refusal, stale next-generation
credentials, peer pending exit, self barrier, writer-before-open and
cancellation preemption custody. `ColdExecIntent` explores **926 generated/342
distinct/depth16**, with explicit stutter and seven invariants. It excludes
physical IRQ/weak memory/register/TLB execution, complete sibling enumeration,
physical reference refinement, affinity/class/funding and full directory removal.
Ten implementation and four specification mutations independently test bounded
qualifiers; neither registered proofs nor modeled roots establish runtime
acceptance or full production equivalence.

The comparison reference is Linux's last-reference-before-`on_cpu` release
rule, not a copied scheduler design:
https://github.com/torvalds/linux/blob/master/kernel/sched/core.c.

#### Independent complete Windows process affinity

`current.rs::set_windows_process_affinity` now uses
`scheduler/process_affinity.rs`, not the genuine directory or a state-free
Scheduler facade. Its non-Copy Grant joins Class55 Change (real process pin,
full handle, checked nonce and captured membership) to all-live Cohort binding
pins before post-pin physical descriptor/context/process/Windows-ABI checks.
Class55 snapshots exact stopped ACKs with membership; only those ACKs exclude
old rows. Missing descriptors, replacement UIDs or desired stop alone refuse,
not implicit drain. Untouched refusal/drop restores only the captured turn.

The IRQ-excluded operation does not yield. Its actual affinity masks, queue
rehome and targeted notification precede process/cohort publication; all pins
survive through queue effects and final unseal outside the process leaf.
`runqueue_policy.rs` now exposes the actual field-free affinity placement and
rehome helpers, retaining admitted HAL topology and existing locality/load
policy. Running custody is not stolen: exclusion marks migration and notifies
its actual owner. Blocked peers are not awakened. A terminal turn may overlap
captured affinity effects; its exact stop survives publication, rehome rejects
terminal custody and physical reuse still refuses the retained cohort/pins.
Actual Local/Remote/Direct transfer CAS now joins Identity/cohort exclusion and
current target-mask admission, so an ordinary source policy cannot spend a
pre-seal mask observation during or after the new independent operation.
Owned affinity explicitly borrows the exact Cohort/UID; a peer that blocks
between masks and rehome is never awakened. Terminal stop between Migrating CAS
and mailbox publication returns rejection, not a panic or resurrection; stale
published records remain subject to existing terminal/generation rejection.
Ordinary cold source callers still retain their genuine enclosing lifetime
owner; this bridge does not claim independent lifetime for every queue caller.
Effect abandonment inherits Change/Cohort poison and retained custody, never
implicit publication or reopen. Native catalog affinity remains fixture-only.

Eleven new affinity witnesses and one terminal mailbox witness use actual
Tickets, physical bindings, process/MM pins and
queue transitions, including actual native Open CPU return custody and a stop
during a queue effect. Native modeled roots/cursor fixtures and Class50 receipt
fixtures are not physical page-table/IPI/CR3/RSP or external completion evidence.
Existing bounded affinity/lifecycle models remain component projections;
complete joint §2.7 refinement, reschedulable cold lifetime, dispatch and genuine
global deletion remain open.

#### Independent Current entry, diagnostic and wait-key fallback

`current.rs` task-ID and user-log fallback, and immutable user wait-binding
admission, now use `stack_resources/context.rs` directly. The fallback requires
this CPU's actual Current allocation-bank pin and exact joined context/start
funding UID under IRQ exclusion; an absent publication never authorizes a
catalog recovery. No context pointer or MM lease escapes. The returned wait
root is only the scalar namespace of an already executing task, not an MM
activation capability or transferable continuation. The physical Current pin
retains the context, descriptor and original MM backing through the root read;
self exec is excluded by the local IRQ scope and remote replacement still owes
actual Current drain. Published logging remains observational, never billing
or dispatch authority. Catalog fallback methods are native fixture projections.

Two new actual Ticket/Current-bank witnesses distinguish physical Current from
an unpinned Current-slot copy, Prepared custody and a foreign Current bank;
they retain physical backing through entry/log/root reads even if observational
metadata is hidden, then deny fallback after pin release. The shared native
factory now initializes its actual scalar MM root under the real Ticket before
publication, matching the constructor boundary rather than assuming installed
context alone publishes a wait namespace. These phase/MM fixtures do not prove
physical switch/CR3/RSP or complete §2.7 arbitration.

#### Independent complete Linux and Windows thread affinity

`current.rs` Linux affinity query/set and Windows current-thread set now use
`scheduler/thread_affinity.rs`, not the directory guard. A non-Copy, non-Send
Grant retains the original physical caller BindingPin and a single-target
Cohort before qualifying descriptor UID, physical ABI and full process handle.
Zero target aliases the exact caller. Discovery copies only nominate a candidate;
neither copied context nor matching process ID authorizes access. Queries retain
both captures until returning; untouched refusal/drop releases only those pins.

The actual IRQ-excluded operation never yields. Its pending target Cohort
excludes complete process/exec cohorts and ordinary source transfer CAS. It keeps
the existing process mask unchanged, validates nonempty online-contained thread
masks against that process mask, and returns InvalidMask for an escape before
any effect (rather than reaching the payload invariant assertion). Captured
Running custody is not stolen; mask exclusion requests reschedule/notification.
Queue effects borrow the actual target Cohort/UID and return before final ACK;
newly Blocked/Retired targets are neither awakened nor reused. Original caller
and target pins survive that ACK, including self-target's two real loans. This
non-yielding custody is not a general retired-user resume exemption.

Twelve actual Ticket/physical-binding/process/queue witnesses cover query,
self/peer set, complete-group overlap, errors, pin release and terminal/block
ordering. The process-mask containment witness prepares its mask through the
actual complete Class55 process Grant, never a post-publication payload reset.
Native modeled MM cursor/Open CPU phase witnesses do not prove HAL/IPI/CR3/RSP.
Catalog affinity methods remain explicit native projections, not the independent
production replacement. Model mappings remain component projections; complete
joint §2.7 refinement, cold producer lifetime, dispatch and genuine global guard
deletion are still required before QEMU/performance/scoped signing.

#### Group withdrawal census and independent thread reservation/cancel

`process_placement/exit_membership.rs` captures immutable exact `(slot, UID)`
group membership under Class55 with a real process-object pin and checked
independent exit nonce. The Waiting seal spends its checked nonce before Join
drain, closes fresh roots/joins/mask changes/new exec censuses, and lets only
already captured joins complete. Capture then requires complete ACK drain.
The exit record is separate from exec's phase/epoch: `exit_pending` during an
existing exec retains its original census; rollback of an unpublished admission
loan cannot reopen exec-owned creation or erase the pending process exit. Metadata
absence or a recycled UID cannot acknowledge withdrawal; effect abandonment
poisons. Captured members survive removal. This is desired withdrawal, not
physical quiescence, terminal consumers, MM/RSP or complete exit cleanup.

Production process termination and self group-exit publish a durable work item
in the existing Class55 process row before any yield. `PendingExit` is only a
short non-yielding admission loan; it transfers the real process-object pin and
original reason into the row. A caller waits with a Copy nonce receipt, never an
RAII owner or backing pin that assumes it will resume. The housekeeping executor
and synchronous waiter can claim one census only after real Join ACKs, existing
terminal turns and placement/exec ownership drain. A successful exact-generation
process mark precedes claim; busy or poisoned placement cannot grant effects.
The independent group worker borrows its original census and an exact physical
member BindingPin through context attribution and terminal producer admission.
That original member loan drops before terminal borrower drain; the producer
now owns source lifetime. Retired members require exact hidden retired binding
or committed stopped UID before skip; own Current is ordered last. A competing
terminal/exec admission explicitly suspends by moving the SAME captured census,
effect phase, nonce and process pin to its durable row. Resume takes that payload,
never recollects live members or reopens creation. Drop still poisons; suspension
is a distinct authorized handback, not abandonment recovery. Both production
waiter and housekeeping retain their original masked CPU scope through effects
and handback/ACK without any directory leaf; housekeeping drains original
terminal producers under that same scope. `service_queued_group_exit` and
`captured_exit_siblings` are deleted; `collect_live_process_sibling_slots` remains
native-catalog-only. Actual born-resource worker/competing reservation/old
borrower witnesses and bounded lifecycle suspension refinements are component
evidence only, not joint frame/MM/RSP/hardware or complete legacy deletion.

`thread_creation.rs` owns actual shared thread reservation/cancellation and
complete suspended publication without Scheduler admission or operation facade.
Own-CPU preemption/IRQ and exact creator pin precede concrete Ticket, allocation,
original MM pin and process attachment. Production legacy thread-slot producer
implementations are deleted; native catalog projections own no physical Ticket.
`thread_creation/work.rs::submit` transfers the complete real reservation and
bootstrap to a distinct per-slot Class50 row **before the first caller safe
point**. Killable users wait with Copy receipts only. Checked nonce survives
actual slot reuse; stale receipt/UID cannot observe another generation's ACK.

Only an actual kill-safe kernel Executor claims sleepable materialization. No
row/identity/resource leaf spans ProcessStateLock, allocation or TEB bytes.
After acquiring process state, original MM activation and a freshly retained
current process/MM pin must match full handle, MM generation and root before
writing. Equal root alone is insufficient. Missing/staged/closed/foreign MM or
TEB initialization error cancels the actual Ticket/attachment/storage before a
failed receipt is published; no user continuation loan or panic-as-cleanup.
Abandoned kernel work returns actual capsule to poison, never silently ACKs,
releases or reopens it. Native abandonment recovery is not production permission.

Final non-Copy CommitGrant captures actual C55 Join and original/current MM
activation **after** materialization. Exact Ticket generation/child/parent UID
and original process qualify the grant. A separate actual parent Cohort retains
its physical context/backing and freezes inherited task affinity through final
child Slot ACK; process mask comes from the complete captured Join, not a stale
reservation snapshot. Funding admission precedes frame effects. Typed exclusive
Ticket FrameWriter owns fixed ABI bytes; context/MM/SIMD/start/frame/binding,
all four gates and membership precede Blocked owner/header generation and Slot
ACK. Only then parent Cohort and durable row complete. Ordinary wake/dispatch
and frame readers refuse even a complete visible constructor prefix until ACK;
Suspended still refuses dispatch after ACK. Cancellation owns the unpublished
Ticket, never vanished/recycled Current. Root/fork, dispatch and full joint
physical refinement remain open; this is not permission to delete the genuine
directory.
Class55 waits retain standalone preemption but mask IRQs only after leaf
acquisition, preserving caller-enabled IPI service while waiting.

Exit membership keeps **committed stopped** detached UIDs as exact tombstones
until the owning census finishes or untouched rollback releases it. This permits
authoritative negative observation after hidden identity clear/slot reuse, never
from descriptor absence or a foreign recycled UID. Release prunes only detached
stopped entries; stopped-but-attached members remain. Effect abandonment keeps
quarantine and does not prune.

The rejected waiter-stack seal candidate remains a native projection only;
production uses durable row custody instead. A BindingPin can retain the waiter's
stack while retirement removes dispatch eligibility, so backing lifetime cannot
prove caller progress. Row work survives requester stop, binding clear and slot
reuse. Dropping a queued census, even before effects, returns its physical pin
to poisoned row custody rather than reopening admission or discarding work.
Only an exact queued desired-stop ACK advances the monotonic completion watermark;
nonce and watermark survive row generation reuse. Absence, foreign UID, timeout,
or metadata stop alone cannot complete a receipt. Exec failure paths release
their census before cancellation can synchronously consume pending exit. Staged
abandonment remains poisoned, not rollback. These are source/native refinements,
not physical MM/RSP ACK or whole-system liveness/hardware acceptance.

The durable executor is separate from the submitting caller; this is the general
queued-work principle described in the official [Linux workqueue introduction](https://docs.kernel.org/core-api/workqueue.html#introduction).
No Linux ABI, pool policy or replacement global lock is imported. Existing
housekeeping remains the fallback executor if a user waiter never resumes.
Full cleanup consumers, root/fork and the genuine SCHEDULER guard still require
independent ownership before complete deletion or QEMU.

#### Process terminal membership and final last-live election

`process_placement/terminal.rs` owns a bounded per-process terminal turn under
existing Class55. A mandatory real process-object pin precedes the leaf;
checked turn nonces, full handle generation and exact `(slot, UID)` authenticate
its immutable membership census. Pending creation `Join`s refuse before nonce
spend. The turn excludes new roots, joins, mask changes, exec censuses and live
member detach, without a raw leaf spanning Identity51, callbacks or destruction.
Other processes retain independent turns; this is not a renamed runtime mutex.

Two questions are intentionally distinct. Pre-withdraw exit authorization may
seal process authority only from a complete exact desired-stop census: hidden
bound retired UIDs count, missing/recycled bindings never count as absence.
Final cleanup instead elects the last **committed terminal member**, so a
retired-but-not-finalized sibling remains potential custody. Identity51's exact
retired flag precedes Class55's stopped bit; recursive retirement of another
member defers rather than consuming an occupied turn. Detached stopped rows
retain the full handle/turn epoch until a real Class50 publication receipt,
qualified by this turn's credential, releases creation and empty-row ownership.
Untouched rollback is exact; post-stop abandonment poisons instead of reopening.

Production retirement and task-exit prechecks no longer enumerate Scheduler
contexts for last-live. The old helper is native-catalog-only. Actual component
witnesses cover creation drain, stale nonces, lifecycle stop, publication receipt,
process pinning and independent processes; selected models remain projections.
Enclosing user creation/publication, group-exit enumeration, exec-image/lifetime
exclusion, own-CPU IRQ custody and hardware return still retain the genuine
directory. These grants do not authorize its deletion or claim QEMU acceptance.

#### Independent deferred terminal stop publication

`terminal_quarantine.rs` owns original terminal reserve/claim and the complete
queue/lifecycle/wait withdrawal through deferred publication. It consumes the
actual non-Copy Terminal50 producer, rechecks exact UID and Identity51/Resources53
binding, preserves the first reason and detach flag across retry/abandonment,
and publishes Retired plus cleared wait/Ready state before deferring the loan.
Neither process detachment, consumer ACK nor hardware quiescence is inferred.
Production quarantine catalog bodies are deleted; explicit native projections
remain. Actual root-born witnesses retain the physical Current bank and original
context/MM/funding through the stop, refuse a foreign UID and duplicate claimant,
and independently deny physical reuse. Their final external completion is an
injected fixture ACK, not execution-return or complete terminal refinement.

`retain_for_finalization` now closes fresh identity/Cohort/CPU admission BEFORE
checking old borrowers. The same original producer retains its UID/nonce and
stop reason without an unlocked defer/reclaim gap; abandonment restores that
stop, never the live task. This closes a real capture-after-count window: the
Terminal50 reservation alone excludes wake/kernel/reset, not every constructor.

#### Independent wake and final terminal effects

`wake_publication.rs` now owns exact Terminal50 -> Identity51 -> Resources53
binding capture through Current-token, assembly-transition or real frame-reader
validation and queue publication. Current wake checks the real bank and never
reads its consumed frame; Transition publishes mailbox custody before clearing
wait. Exact reader/binding loans survive policy and queue effects. Invalid wake
reserves the same original stop before dropping those loans, then transfers it
to finalization outside IRQ context or defers it within IRQ context.

`terminal_publication.rs` owns complete stop, borrower drain, actual process
terminal election, exact Linux cleanup, pending SIGCHLD transfer, donation
release, wait withdrawal, original process-detach payload capture and last dual
consumer publication. All old source borrowers must drain except the original
own-CPU Current permit/bank. Busy election or foreign execution defers the SAME
producer. Linux transfer retains exact target UID/binding through pending bits,
mask handling and wake; source bits clear only after successful transfer. The
original process turn survives all effects and admits final publication.
Production finalization and wake catalog effect bodies are deleted; isolated
native projections remain. Real root/thread/activation witnesses test this
pipeline, but modeled Current/topology and injected external ACK are not physical
return, TLB completion or external revocation acceptance.

The remaining explicit unsafe adapters require their original same-CPU
IRQ/preemption AND dispatch serializer. Running->Blocked/Local queue commits
still depend on that serializer; the process-wide donation scan is likewise
still qualified by it. These engines do not authorize deleting that guard.
Immutable sibling quiescence, complete Current/IPC/IRQ/dispatch arbitration,
source-refining joint §2.7 models and genuine Scheduler guard deletion remain.

#### Producer-qualified terminal custody before withdrawal

`retirement_cleanup/producer.rs` now owns the production Class50 terminal
transaction. Its non-Copy loan reserves the row before queue withdrawal or
lifecycle effects. Checked generation and producer/claim nonces authenticate
UID, original reason and process-detach intent across deferral. Consuming a
reason alone cannot claim, defer or publish production retirement. A second
claimant refuses; untouched claim abandonment restores only its exact deferred
loan; effect abandonment keeps poisoned occupancy. Final cleanup and kernel
revocation payloads become visible together as the producer's last operation.
Both exact consumer ACKs precede terminal reset and physical reuse.

The directory adapter checks hidden UID under Identity51 joined to Resources53
after reserving Class50, with no Class50 leaf spanning this join or callbacks.
Constructor, image writer, irreversible close and physical reuse prefixes refuse;
absent descriptors do not grant authority. Native catalog arrays are explicit
projections; real row tests isolate loan, compatibility-bit and consumer gates.

This is terminal metadata/lifetime custody, **not** process-last-live arbitration,
exec-image exclusion, own-CPU/IRQ permission or physical MM/RSP ACK. The genuine
directory remains around those enclosing transactions. Process membership
publication and concurrent last-live decisions must be replaced before their
callers or the directory guard are removed; no QEMU/product equivalence is
claimed by this component's native/model evidence.

#### Frozen exec membership and independent input admission

`process_placement/exec_membership.rs::ExecMembership` captures the exact
process generation, full exec reservation/intent credential, target UID and
bounded member census under Class55. It physically pins the process object
outside the leaf. Capture requires open placement, zero outstanding creation
joins, an existing target member and a checked nonwrapping nonce. Pending
creation or affinity transactions refuse without spending that nonce.

The exclusive exec phase denies root/child membership publication and process
mask replacement until cancellation or completed image publication. Removal
may shrink the live ledger, but cannot erase captured sibling UIDs or reopen
creation. Finish requires the exact finalized intent certificate and released
FrameWriter before spending the census epoch. Before-image abandonment releases
only its exact epoch; abandonment after successful process staging poisons the ledger. Leaf scope never spans
Identity/Resources/queue acquisition, process callbacks, hardware or pin Drop.

Self and peer input admission no longer acquire the scheduler directory.
Copied Linux leader candidates are observations only; own-CPU preemption/IRQ
scope and `Intent::begin` authenticate descriptor/start/PID/TID/process handle
and the identity/resource producer before any image permission. Nonleader or
foreign inputs refuse without a new public errno/wire format. Production image
leases retain the census through staging, publication, FrameWriter Drop and
intent completion; the old image-time sibling rescans are fixture-only.

`exec_transaction/quiescence.rs` now implements the actual immutable sibling
barrier. Exact intent/target admission and the original captured UID/handle
precede effects; each live sibling transfers the actual original non-Copy
terminal producer with detach intent through admission closure, borrower drain,
process election and dual-consumer publication. Deferred/busy election remains
pending even at ZERO execution refs. A retired member whose original detach
removed its handle still waits for physical Current/Transition or old execution
references; no second stop/election is inferred. External requests cannot spend
any sibling producer when the census includes their own live caller stack.

Production `quiesce_captured_exec` is only an adapter; its old effect body and
unused production sibling/admission helpers are gone. Native catalog projections
remain explicitly separate. Four actual root/thread/census witnesses exercise
retired borrower retries, original detach payload, busy election without refs,
caller-stack and mode refusal. Native entry/external ACK are not hardware proof.
The two Current callers still retain the genuine global/IRQ/preemption guard;
it still excludes concurrent reaping of already-published retired observations.
Do not reopen the terminal capture-after-count window with a new retired binding
pin or treat descriptor absence as a hardware ACK. Guard-free retired-observation
arbitration and joint CPU/MM/terminal refinement remain required before deletion.

#### State-free facade / boot and diagnostic custody

Production `Scheduler` now has no data fields (`size_of::<Scheduler>() == 0`
is compiler-checked). Native catalog fixtures remain isolated. AP idle CPU
placement derives from the admitted runqueue idle tag and fixed boot slot;
slot 0's idle tag marks completed bootstrap work, not an AP CPU mapping. Tick
configuration lives in `boot_execution`, published before boot admission and
preserving the raw zero PIT encoding independently of task weights.

`runtime_profile/owner` owns CPU-local counter cells and exact-UID per-slot
migration history under scalar Class56 leaves. IRQ exclusion precedes local CPU
sampling; no leaf crosses resource observations, allocation or rendering.
Ordinary not-due report probes only read the epoch. A nonblocking reporter CAS
serializes destructive collection, with one CPU leaf held at a time. Collection
is an approximate multi-CPU diagnostic window, not an atomic execution snapshot.
Max hold time, caller and attributed time stay one tuple. Report drains reset
window segments but retain a separate saturating lifetime phase clock, so a
remote drain cannot erase an open turn's baseline. Slot-based runtime ranking
retains the prior diagnostic observation semantics; it is not a lifetime grant.
Eight native witnesses and focused mutants cover these limited obligations;
they do not refine physical IF/APIC/CR3/RSP or complete lifecycle arbitration.

**Data relocation is not serializer deletion.** The genuine global guard still
arbitrates cold creation/activation/exec/exit and reaper/reuse operations, including
complete task-slot versus domain publication. A fresh facade is not an independently qualified transaction grant;
production call sites remain guarded. Do not replace the guard with an empty
mutex of another name or delete it because the facade is zero-sized.

#### Independent cold domain admission and captured membership

`domain_admission` now serializes only bounded cross-domain deadline utilization,
policy lookup/replacement and descriptor-slot allocation. It reads Domain49,
not Scheduler context enumeration. Class57 is a cold registry transaction owner,
not a leaf and not a renamed directory guard: its production scopes cannot cross
resource/process owners, allocation, external callbacks or hardware. Lock-class
numbers are identifiers, not numeric ranks: registry57 → context48 → domain49;
hot funding pairs retain context48 → domain49 and never acquire the registry.
Standalone preemption precedes registry waiting; caller-enabled IPIs remain
serviceable until acquisition, then IRQ masking protects the transaction.

The non-Copy returned `Admission` retains exact domain slot/generation/policy
membership before leaving selection. Both user-root constructors retain it
through context publication. Runtime context initialization validates and adds
its own member before the temporary capture can disappear; occupied context cells
cannot spend another member. Closing denies fresh funding but does not erase
members. Exact context identity release drains its member, and replacement needs
zero members **and** zero active captured intervals. Metadata disappearance alone
is not proof of absence. Domain generations use checked reservation: exhaustion
refuses without wrapping or publishing a descriptor.

Eight native witnesses plus focused mutants cover selection/publication gap,
duplicate captures, closing/exact release, foreign policy/generation/identity,
CPU-mask utilization, full slots, competing admissions and exhaustion. A native
incomplete/orphan-interval fault witness independently requires interval refusal
even with zero members, so membership cannot mask that gate. Existing
budget models remain bounded projections, not complete registry/member, weak
memory, creation/exit or physical return refinements. This transfer does not
qualify fresh state-free Scheduler facades, replace constructor/reaper/activation
transactions or authorize deletion of the genuine global guard.

#### User-root producer custody before descriptor visibility

Both production user-root constructors now reserve an exact slot/UID/generation
Ticket before allocating or initializing stack bytes. Reserved frame writers,
context installation and start publication borrow that producer, not a copied
free-slot observation or a fresh Scheduler facade. Shared `root_slots` storage
preparation retains the thread constructor's canary/size semantics; allocation,
byte initialization and detached Vec destruction occur outside resource leaves.
A refused process publication cancels only the unpublished Reserved producer,
drains its stack, then releases reservation. Committing/abandoned producers
cannot use cancellation to reopen a partial image.

Both production root/fork routes now use independent `root_creation`, not a
Scheduler method or the global guard. The non-yielding own-CPU scope captures
actual Current backing and the original user BindingPin/Cohort or kernel
Executor. A supplied policy remains userspace authority; inherited policy and
process mask are requalified under the original parent binding/Cohort through
publication. A same-actor parent borrows that cohort rather than recapturing it.
Domain admission and actual runtime budget initialization precede PID visibility;
foreign orphan runtime refusal neither publishes nor releases foreign funding.

`RootPublication::prepare` joins the actual Ticket to checked-nonce Class55
RootJoin **before fallible materialization or PID visibility**, then seals the
original Class54 SpawnReservation outside Placement55. The table stores the
exact transaction/handle/member/producer-generation/root-nonce snapshot, but
only the actual non-Copy RootSpawnLoan can spend its live seal. The original
copied receipt cannot publish or cancel it. Private state and Windows TEB failure
cancel the unpub process/slot owners; no state/frame/destruction spans a leaf.
Actual process backing is pinned under Class54 **before** PID visibility release,
not recaptured after publication. The logical join exists before insertion. Publication requires actual
Committing context/process/UID, registers that exact member, admits Blocked, then
consumes the real Ticket ACK under Identity51→Resources53. Its non-Copy receipt
binds slot, UID and producer generation; only its matching RootJoin may consume
the final membership ACK. The root capsule also binds the actual frame header
and retains a real physical bank loan **before** releasing Slot ACK. Optional
runnable wake and notification follow both ACKs with that loan still live;
terminal overlap refuses reuse and never resurrects a retired owner. Suspended
publication finishes both ACKs but grants no wake or fresh execution permission.
The complete descriptor, fixed-ABI frame, MM context pin, SIMD/TLS, funding and
PagerChargeStamp, four gates and task/process masks precede those ACKs.

The interval between Slot ACK and membership ACK remains closed: actual physical
wake, execution, cold binding and terminal qualification refuse a pending root
row. This negative exclusion is never a grant inferred from an absent row; all
existing physical tuple and owner qualifiers still apply. Queries take
Identity51/Resources53→Placement55; the membership ACK inspects the owned receipt
without taking a lower leaf. Exit census waits for the real join, and exec/child
admission cannot bypass it. Untouched unpublished abandonment atomically cancels
the exact Class54 transaction together with its seal before rolling back
Placement55 and physical slot custody. There is no unsealed interval in which a
copied receipt can republish while slot rollback is still pending. Newly visible abandonment, even
before registration, retains the actual process pin and root join in poison
custody; registered abandonment also retains its member. Neither silently
reopens process construction or reclaim. Nonce exhaustion refuses
without wrapping or spending the original Ticket.

Complete root/fork birth and parent-policy/input capture now have independent
owners. They do not replace general process exec/exit arbitration, IRQ/dispatch
return, hardware/class/funding joint refinement or the genuine remaining guard.
Already-materialized fork state preserves its existing semantics; native modeled
MM namespaces and explicit external-cleanup ACK injection are not hardware proof.

Five native witnesses exercise the real preparation/reservation/final-ACK owner,
including root UID zero, actual canaries, competing producers, complete physical
process/descriptor/membership prefix denial and commit-versus-cancel refusal.
Eight additional native witnesses exercise actual RootPublication/RootJoin,
separate Slot/membership ACKs, foreign receipt rejection, exec/exit exclusion,
poison pin retention and checked nonce exhaustion; fourteen implementation
mutants bind these qualifiers. Twelve further actual shared constructor witnesses
exercise original reservation visibility, copied-receipt refusal, Windows failure,
visible-unregistered poison, parent loans, suspended ACK, funded/unbudgeted
refusal, terminal overlap and same-UID reuse. Native catalog constructor branches
remain explicit fixture-only projections with no unreachable production bodies;
source/compiler checks bind **both production root routes** to the real capsule. Existing admission models are bounded projections, not full
creation/process-publication or §2.7 guard-deletion refinement.

#### Dynamic kernel producer versus bootstrap/AP custody

The production dynamic kernel constructor now uses the same exact reservation
before stack bytes. Its borrowed `init_reserved_kernel_entry_context` writer
retains the existing trusted fixed entry/trailer ABI; context/start publication
uses the committing producer. Full identity/SIMD/affinity prefix precedes
Blocked admission, exact producer ACK and runnable wake. A complete kernel
UID-zero descriptor with no user MM/process is valid kernel metadata, never a
shortcut around reservation. Three native witnesses exercise the actual
reserved writer/bytes, reject generic boot or nested writers without byte
effects, and prove complete kernel metadata remains sealed through Blocked ACK.
No new CPU/MM/funding/class/hardware grant is inferred from that frame.

`Thread::start` still uses the genuine guard. BSP reset and fixed AP idle
creation/publication remain different transactions, not ordinary dynamic
allocation; AP live-stack/Running bank publication cannot use a Blocked ACK.
The reaper audit found its dynamic allocation detachment last, after old queue,
context/domain, identity, policy and diagnostic reset. That ordering is useful
but not a substitute for complete producer-last teardown and boot/activation/
exit arbitration. Native catalog kernel allocation remains fixture-only;
source/compiler routing binds the production dynamic constructor. These slices
do not authorize an unqualified zero-sized facade or global guard deletion.

#### Initial bank admission versus a constructor-visible prefix

The BSP constructs AP idle metadata/current banks while target CPUs remain
OnlineParked; `SchedulerReady` is released only after the complete BSP/AP image.
The later AP entry installs CR3/TSS/syscall stack. Those separate publications
are not an incoming-RSP or hardware ACK and remain genuinely serialized.
Auditing this path found that initial execution-reference acquisition and the
initial physical CPU pin lacked the ordinary constructor/writer exclusions.
They must not rely on the enclosing global guard for these independent gates.

`execution::initial` now joins Identity51→Resources53 before its checked atomic
reference increment. Any reservation phase, active frame writer or irreversible
close refuses before a reference is spent. The callback is only atomic reference
acquisition. Initial physical pin qualification independently rejects all three
under Resources53, checks exact start/binding UID and constructs the non-Copy
StackPin outside the leaf. Refusal has no CPU-bank effect. Two native witnesses
inject each constructor phase and writer/close separately against complete
kernel metadata, testing lifecycle and physical acquisition independently so
one cannot mask a removed predicate in the other. Six mutants anchor those
independent predicates. Recovery is native fixture reset, not production reopen.

This closes the prefix/writer bypass in initial pin admission, not the complete
CPU-bank initialization transaction: exact boot/lifecycle generation, unique CPU
initial publication, placement/class/funding and hardware admission still need
separate credentials before replacing bootstrap/global serialization. Models
remain bounded projections; no physical bank/CR3/TSS/RSP refinement is claimed.

#### Independent complete exec image publication

`exec_transaction/publication.rs` publishes both self and peer images under the
original non-Copy intent/image, frozen census, exact FrameWriter and staged MM
owner. No Scheduler or global guard constructs this image. The typed lease opens
its own unchanged-Current CPU control turn, borrows that turn's existing
preemption scope, and authenticates the actual original Current backing bank.
Its return retains the same Current; no next-task scratch, sibling rescan or
fresh dispatch grant is involved. Target retirement/deferred cleanup, exact
physical context/start/UID/process binding, caller mode, Linux PID/TID and peer
quiescence are checked before effects. Image failure remains owned by the
existing durable intent/quarantine, not automatic rollback.

The original writer bounds own all peer fixed-ABI frame bytes. Self exec never
rewrites its consumed frame, saved RSP or runnable owner, and never advertises
Ready. Only exec ABI/console/user-stack fields change in the real context;
funding, process generation, Windows state and task/process masks are preserved.
Original signal mask survives while pending signal/SIGCHLD bits clear; exact
writer-qualified Linux/TLS replacement remains under its own leaf. Actual SIMD,
descriptor, backing/frame epoch and identity metadata publication precede the
once-only MM/CPU publication receipt. MM activation and captured placement remain
in the independent control turn until unchanged return ACK. Physical CR3/FS
installation occurs only in the boot image; native architecture namespaces do
not acknowledge it. Per-CPU state, not legacy scheduler scratch, identifies the
MM activation receiver. Deferred target notification precedes scope restoration.

Four root-born component witnesses and thirteen independent mutations exercise
real process staging/finalization, peer frame/metadata/Linux/TLS, exact funding
and affinity preservation, foreign PID/TID/mode and copied-bank rejection, and
no consumed-self writes/Ready publication. Native Current/initial topology and
HAL are explicit modeled inputs, not complete executing-bank/funding, physical
CR3/FS/RSP acceptance or joint §2.7 refinement. The old direct catalog image
constructors are native-only projections with their production branches deleted.
Captured sibling quiescence, enclosing terminal transactions and remaining
IRQ/dispatch/global authority still need independent transfer before deletion.

#### Independent suspended activation and authority commit

`scheduler/activation.rs` now owns production single/batch/registry-qualified
activation, not a Scheduler directory turn. Exact nonzero unique target UIDs
resolve from published user metadata (a bounded scan, not hint authority); the
Cohort pins every suspended binding and seals an immutable original slot/UID
array. Production commit no longer constructs or delegates to Scheduler:
`activation/publication.rs` independently authenticates original own-CPU Current
and physical bank, acquires exact published frame tokens, original backing pins
and actual FrameReaders, and rechecks epochs/generations/RSP before complete
frame validation. These real loans survive the bounded infallible allocation-free
caller registry callback and every queue/policy effect; no raw identity, resource
or policy leaf spans the callback. All task effects precede one admission Release;
readers then pins drain before deferred notification. Native catalog commit is
an explicit test-only projection, not the production physical implementation.

Activation now checks suspension under Identity51 before seal/pin publication,
not a later copied predicate. Concurrent terminal stop may hide metadata but
retains exact bound UID; owned unsuspend cannot reawaken it and skips terminal
members. Lost binding/suspension or Running in the sealed prefix remains
corruption; a desired terminal stop is not such a mismatch. Before-task-effects
callback unwind keeps members suspended and releases exact refs; registry
callbacks remain required to be infallible, with no promise of userspace rollback
of partially consumed authority. Partial task publication poisons admission.

Individual affinity replacement previously checked Cohort busy only under54,
which did not serialize with the member seal under51. It now joins exact UID and
operation-qualified Cohort permission under Identity51 before Affinity54 stores.
A conflicting public mask update refuses before epoch/masks and returns existing
MissingTask; owned group-mask updates retain their separate credential. Producer
initialization is still ticket/image-qualified, not a public replacement shortcut.

Empty-cohort-priority observation likewise was not an insertion reservation.
Competing disjoint activations now arbitrate actual queue/count under each CPU's
Policy42 leaf. Full/duplicate/exhausted priority custody degrades to ordinary
spawn hints without panic, and never grants Running; pending Cohort still gates
the real CAS. Hint insertion precedes its pending notification under the same
leaf. Whole group admission remains atomic despite optional latency priority.

Actual local/remote publication uses unchanged affinity admission and checks
real runnable membership, not merely a copied wake outcome. JobStopped and
ExecQuiesced members remain Blocked without queue or priority grants. Strict
System classification uses the published weight flag plus inherited donation;
frame-qualified same-band ready floor preserves sleeper latency. Ordinary local
spawn fallback recomputes this floor after wake before applying the existing IPC
donation bonus; priority insertion does not invent that donation. Original-order
FIFO and checked count/advertisement remain under the per-CPU Policy leaf.

Eleven additional actual root-born component witnesses and twelve new mutations
cover frame/binding loans across callback, invalid frame before consumption,
hidden terminal, unwind/retry, copied Current bank refusal, stop gates, exact
capacity/replay, saturated FIFO, remote mailbox and strict class/donation floors.
Two original callback/terminal mutation obligations now execute against this
shared physical producer; existing seven catalog witnesses/nine original mutants
remain explicit projections. Native Current/MM/HAL namespace inputs and injected
terminal cleanup are not hardware grants. Existing activation/identity/frame/ref
models are bounded components, not complete registry-policy, process
visibility/exit, quarantine cleanup, captured class/funding or
CR3/TSS/TLS/SIMD/RSP acceptance. Neither this complete producer transfer nor its
component evidence permits deleting the remaining genuine global guard.

#### Kernel creation versus producer-last drained reuse

Production `Thread::start` delegates to `scheduler/kernel_creation.rs`, not the
Scheduler directory. The scheduled-kernel-main executive calls occur after the
complete initial image. The factory directly owns the actual Ticket, typed
FrameWriter, context/runtime/SIMD/identity and four-gate publication; production
facade construction and its old initialization method are deleted. Before
Blocked Slot ACK releases the Ticket, exact committing epoch/UID/geometry/MM
qualification captures a physical allocation-bank loan. Reuse/context reset
cannot reclaim or retag the child during ACK→wake, even if a concurrent terminal
owner stops it. Wake may refuse without resurrection or an invented liveness
ACK. Backing never grants dispatch or user-resume permission. Own-CPU standalone
preemption/IRQ custody is independent; target notification follows all leaves.
The initial vruntime samples eligible local/direct frame-qualified observations
under actual pin/reader custody and preserves the existing penalty; it is not
billing or dispatch authority. Native topology/root namespaces are fixture
inputs, not physical CR3/IPI evidence. Actual producer/bank/queue/frame component
witnesses remain distinct from production hardware acceptance and joint §2.7
refinement; bootstrap/AP admission remains separate.

Physical vacancy is a transaction, not a `context == None` observation.
`stack_resources/reuse.rs` joins hidden/visible UID binding under Identity51 to
Resources53, captures checked persistent reuse nonce, and excludes producers,
writers/readers, refs and pins. Its non-Copy Closing→Detached receipt retains
occupancy through old queue/context/identity/policy/budget resets and owned
storage detach. Final exact drained ACK clears hidden old binding while keeping
exec generation, then removes the physical gate. Old allocation destruction
remains outside these leaves. Reservation separately refuses active reclaim,
unowned close, orphan MM pin and readers; an empty descriptor cannot erase them.
Unowned close refuses every reservation phase. Abandonment poisons occupancy;
there is no automatic retry/reopen or promised abandoned-resource cleanup.

`retirement_cleanup/reuse.rs` independently seals a drained exact terminal row
with checked reset nonce and original row generation. Producer, deferred,
external and kernel custody each deny reset. Before-effects Drop releases only
its own nonce/generation; post-effects Drop retains quarantine. Unowned row reset
and new terminal/deferred publication refuse the seal, including the otherwise
vacant owned-reset prefix. In production reclaim, terminal seal precedes physical
receipt; all reset effects precede owned storage detach, then terminal completion
precedes physical vacancy ACK. Class50 is never nested with Identity51/Resources53.
A failed physical claim rolls back only the untouched terminal seal.

Ten real component witnesses and twelve independent predicate mutants project
these boundaries; injected native recovery is not production reopening. Existing
admission/terminal/cross-CPU models do not establish complete process membership,
group exit, producer-last side-effect publication, eventual quarantine cleanup,
class/captured-funding CR3/TSS/TLS/SIMD/RSP return or hardware acceptance. The
remaining directory remains genuine for those enclosing runtime transactions;
this boundary does not authorize its deletion or early QEMU acceptance.

#### One-shot BSP/AP image and generation-qualified initial producers

Boot and AP entry no longer borrow the global Scheduler directory. A private
one-shot image owner admits only the BSP, an uninitialized image and exact
OnlineParked CPU generations. Its non-Send image key qualifies the otherwise
state-free facade; it cannot manufacture runtime directory authority. There is
no spin/mutex wait and no supported image reset/restart. Each initial CPU producer
captures exact CPU/slot/UID/lifecycle generation, refuses aliases and admits
Effects→Publishing atomically before bank effects. Partial producer/image Drop
poisons rather than resetting or destroying captured resources.

BSP reset and fixed AP idle creation now borrow these producers; AP stack bounds
come only from the exact HAL generation. Typed initial bank publication verifies
Running CPU/generation and kernel context, retains lifecycle/physical Current
custody before releasing Current slot/active words, then ACKs its unique producer.
In particular, BSP initial Current banks now precede Current-only SIMD save;
the earlier path had no such BSP bank initialization. All discovered CPU ACKs
must precede one Ready image Release and `SCHEDULER_INITIALIZED`; each HAL
SchedulerReady transition retains its captured generation. Runtime directory
entry no longer silently creates a BSP Current and rejects an incomplete image.

BSP architectural preparation and AP entry each use a qualified own-CPU turn,
not the directory guard. AP entry is an exact unique SchedulerReady-generation
producer, captures placement before CR3/TSS/syscall-stack effects, and completes
only after local TLB/clockevent admission and the same generation's Online
transition. No image/pin/placement ACK asserts an incoming RSP or physical HAL
acknowledgement. The BSP remains on the HAL boot stack until saved-context
restore; APs already retain the generation-assigned static HAL stack.

Seven native owner witnesses exercise competing claim, complete-image prefix,
exact tuple/roles, duplicate publication/ACK/entry, poison, UID alias and bounds.
Native ACKs are state projections, not physical bank/CR3/TSS/interrupt/assembly
acceptance; source/compiler checks bind the production routing. Existing
CpuOnlineLifecycle/SchedulerCpuOwnership models are bounded projections, not new
complete bootstrap or funding/hardware refinement. This deletes two genuine
boot/AP directory call-site dependencies, not the remaining runtime global guard:
creation/activation/exec/exit/reaper arbitration and full captured class/funding
hardware return still require completion before guard deletion/QEMU acceptance.

#### Running return / Blocked wake CAS arbitration

`runqueue/return_publication.rs` owns the actual outgoing queue transaction:
under the original CPU queue leaf, each failed owner CAS rolls back ONLY its
provisional membership and re-evaluates wait/wake run intent. Terminal wins
without resurrection. A successful Local publication binds the exact owner
generation; Blocked preserves the original wait. Blocked local wakes use the
same retry/rollback discipline instead of panicking on concurrent owner change.
A wake observing a retained physical Current bank rechecks Blocked after clearing
the wait and publishes queue custody if return already committed, without reading
the consumed frame. Current/Transition banks still exclude fresh execution.

Four native queue projections exercise actual CAS/queue operations and directly
inspect published membership, not the native broad candidate-scan projection.
The original root/bank wake witness models earlier queue claim and uses actual
Blocked return; it does not supply MM activation or physical register acceptance.
These component witnesses do not transfer the enclosing IRQ/dispatch serializer,
fast-IPC fallback, accounting, architecture return or complete §2.7 refinement.
The genuine global guard remains required and is not removed by this boundary.

#### Original Current wait loan and fused pager cancellation

`current_wait.rs::CurrentWait` captures an exact non-Send UID/backing loan only
from the original physical Current bank under the caller's masked same-CPU IRQ
scope. Running is a separate queue qualification, never lifetime authority.
Terminal50 excludes fresh loans before Identity51 -> Resources53 capture; an
already captured loan survives terminal competition. Arm uses a fallible actual
wait CAS, not an assertion after stale admission. Wake winning the arm refuses
commit; terminal never re-arms or resurrects. Fused pager publication retains
ONE original loan through commit and exact-token rollback. Its original pager
token is retained before stop can erase the published reason; even terminal
cancellation refuses a foreign token instead of treating every token as ACK.
It acknowledges exact raced terminal cancellation without wake and drops the
loan before scheduling trap.

Current arm/cancel and IRQ ordinary/reply commits have no catalog fallback.
Missing physical custody fails closed; a valid already committed loan reports
no new commit. Production legacy commit implementation is deleted; native
catalog projections remain explicit and do not supply physical Current grants.
Seven real root/bank/atomic witnesses replace the former raw current-wait and
fallback projections. Earlier queue claim and IRQ entry are modeled; full
process terminal detach and physical/MM return are not established by them.
Complete fast IPC, dispatcher return and joint §2.7 evidence remain outstanding.

#### Original dispatch return and failed fast IPC peer claim

`current_return.rs::CurrentReturn` borrows the original unique masked
`CpuDispatchTurn` and its actual physical Current bank. Exact resource UID,
context flags and frame geometry qualify the loan before byte access. A scalar
Current or a different nonzero allocation pin is not the bank. Terminal may
close new admission while this original bank still lends its old binding; no
fresh post-stop pin is acquired. Rust borrowing keeps Current publication from
consuming the turn until this non-Send loan drops.

Production outgoing accounting is removed from Scheduler. The loan consumes
its original captured funding interval once, preserves budget-before-fairness
and the minimum voluntary charge, and refreshes the live canary under retained
bank custody before frame publication. Wait/terminal owns run-intent cancellation:
frame refresh cannot erase a raced completed wait. Actual return CAS publishes
one exact queue receipt after account/frame settlement. That receipt is not a
fresh Local/Blocked observation or a suspended-frame grant while Current lives.

The software IRQ carries ONE loan through fast IPC and its failed generation
claim into ordinary fallback. No repeated charge, frame refresh, queue generation
advance or acceptance of arbitrary Blocked custody is needed. Cached validation
accepts only this producer's exact original IRQ RSP, never a different pointer. Fast outgoing
publication now uses the same CAS/affinity rule, not the old single-CAS Blocked
producer. Incoming selection, budget/placement, SIMD/MM/architecture effects
and the enclosing genuine guard remain separate authorities still to transfer.
A production phase-profile compile exposed a stale diagnostic call to the
already native-only minimum-slice comparator. The timer diagnostic now rechecks
raw CFS ordering, never a dispatch decision; only the published local picker
owns the production slice/burst rule. The native comparator stays native-only.

Nine actual root/bank/turn/interval witnesses and twelve mutations cover the
original fallback, bank and independent allocation-pin refusal, replaced Current,
foreign geometry, wake preservation, live guard refresh and retained terminal
loan. Earlier queue selection, IRQ user-frame inputs/MM and native phase cleanup
are explicitly modeled, not complete joint or hardware-return proof. These
component refinements do NOT authorize deleting the remaining global guard.

#### Exact selected frame, funding and queue claim transaction

`dispatch_claim.rs::DispatchFrame` independently borrows the original masked
CPU turn and retains the selected expected UID, exact saved RSP, owner generation,
frame epoch and real allocation pin. Its actual FrameReader precedes header
validation, including the live allocated canary rather than only the old published
flag. Local, consumed Direct FIFO and retained original Current are distinct
custody modes. The reader drops after validation; **both** actual owner-CAS branches
requalify that original token INSIDE Identity/Resources/placement claim exclusion.
A same-owner-generation frame replacement between validation and claim therefore
refuses. An allocation reference alone or an earlier discarded validation boolean
is not this final frame grant.

`DispatchFunding` retains the original Context→Domain/reply pin in the selected
transaction, not a CPU scratch cell. Preparation matures refills without debiting
runtime. The final credit/liveness check uses that original pinned stamp, never
re-resolves donation; Context/Domain locks do not cross the queue claim. A closed
or exhausted funder refuses new execution while old executing intervals still
settle. Successful claim transfers the exact allocation and funding into CPU
preparation; stale frame/owner, another reader, terminal or affinity refusal drops
them without staging a grant. The existing Running interval opening consumes
that prepared funding exactly once. This does not reserve all cross-CPU credit
or prove a hardware/MM/IRQ joint return.

General dispatch, fast IPC and actual Current/idle fallback use this engine. The
production Scheduler budget adapter and old fallback effect body are deleted;
native catalog budget/idle projections remain separate, without an unused
production effect branch. Selection policy, post-claim interval opening and
remaining Current/IPC lifecycle arbitration still enter the genuine global
guard. The claimed architecture/SIMD return below has its independent original
receipt, but the enclosing dispatcher remains guarded: this is NOT complete
legacy deletion.
Seventeen actual root/bank/turn/resource/funding/claim witnesses cover these
boundaries; prior IRQ/loader VA inputs and native bank/phase recovery are explicit
models, not hardware RSP acceptance. Component source mappings and mutations do
not substitute for the complete joint refinements required below.

#### Original claimed architecture and SIMD return

The successful validated claim stores a non-Copy `ClaimedReturn` in its unique
original masked `CpuDispatchTurn`, after transferring physical allocation and
funding to CPU banks. It retains the exact validated epoch/RSP/UID, actual next
Running generation, CS-derived return mode read under the ORIGINAL FrameReader,
and original Current architecture snapshot. A copied `SchedulerDispatch` is
only checked against that receipt; it cannot recreate one.

`return_architecture.rs::ReturnArchitecture` borrows this same turn and receipt.
It qualifies actual Prepared/execution banks, exact resource binding/geometry,
original placement and original MM activation root. It acquires no new frame,
allocation, MM clone or activation after claim. Register installation preserves
same-task elision, same-root CR3 elision, full user segment/TLS restoration and
same-root kernel fast-IPC FS/GS delta; every IRQ leaf restores SIMD through the
same receipt, including same-task turns. Original architecture phase admits
once BEFORE register writes, reaches ready only after SIMD, and actual CPU
publication requires that ready receipt for the exact selected slot. Failed or
abandoned hardware preparation remains quarantined by the original turn.

The legacy Scheduler hot architecture bodies and production SIMD restore body
are deleted, not renamed or hidden. Native SIMD projection is separate. Boot
initial-image and live pager continuation architecture still have their separate
existing original scopes; these are not granted by a copied dispatch result.
Eleven actual root/bank/MM/placement/receipt witnesses cover Current, Local,
Direct, foreign copied fields, changed frame/UID/geometry/banks/scalar, terminal
refusal and duplicate architecture admission. Native cleanup/phase completion
are explicit MODELS, not hardware register or RSP ACK. Selection/postclaim,
Current/IPC lifecycle, remaining boot/pager scopes and full joint hardware/MM/
terminal refinement still need transfer/evidence before genuine guard deletion.

#### Original exec census versus physical reclaim admission

`process_placement/reclaim.rs` now arbitrates the ORIGINAL frozen exec census
against actual Resource53 reclaim admission under Resource53 → Class55 order.
`ReclaimAdmission` is a borrowed actual resource turn with its checked original
reuse nonce, not a copied context/slot/UID grant. Class55 publishes the exact
member reservation before the same Resource53 turn installs its producer-last
Reuse seal. Census-first refuses reclaim BEFORE nonce/close publication;
reclaim-first refuses a new census. A read-only busy check alone cannot close
this bidirectional admission window. No new execution, allocation or MM pin is
acquired after stop, so terminal borrower counting is not reopened.

Physical context/process/MM release checks the original member reservation and
resource nonce inside the actual Resource leaf before Class55 unregister. A
frozen census also denies descriptor reset. Logical unregister during EXEC
retains the exact member tombstone through original census release; exit pruning
cannot erase that fence. Physical unregistration removes the old member from
future censuses, but it is NOT physical Reuse completion: original Reuse remains
closed through allocation detach and external completion. Abandoned Reuse keeps
its membership reservation and physical poison; absence cannot reopen census or
creators. Kernel/unpublished resources with no process member are not fabricated
census grants.

Seven actual born root/thread/resource witnesses include both admission orders,
a concurrent exactly-one-winner race, logical removal, original nonce refusal,
nonce exhaustion and abandonment. Terminal consumer ACK, negative-input faults
and fixture-only recovery are explicit native models, not hardware/RSP ACK or
joint source refinement. Existing original exit-tombstone and terminal-detach
mutation obligations remain on the same Class55 effects. The genuine global
guard and both Current exec callers remain required by terminal/IRQ/dispatch
boundaries; this resource admission transfer is NOT complete guard deletion.

#### Original terminal census / process donation withdrawal

The same physical admission boundary now retains the ORIGINAL terminal process
census: real Reuse reserves before terminal capture, and an existing terminal
turn refuses physical reclaim/reset of its captured members. Logical unregister
retains stopped member tombstones through original terminal release. Overlapping
exec/terminal/exit release transfers the remaining census fence instead of
pruning a still-captured UID; effect abandonment keeps the terminal poison.

Deleted the production final-process all-slot PID/context/start scan. The final
stopped `terminal::Turn` lends its complete immutable member IDs while retaining
actual process/resource admission custody; donation withdrawal runs OUTSIDE
Class55 and completes before the original producer publishes either consumer.
Kernel resources are not invented process members; actual user membership is
published by root/thread admission. A reserved/not-yet-stopped turn refuses the
final donation census. Copied PID/UID, terminal metadata or a freshly read slot
cannot grant this process-wide cleanup.

Five additional actual resource witnesses cover terminal-first and reclaim-first,
concurrent exactly-one-winner, overlapping census release in both orders and
actual final terminal withdrawal of a retained stopped-member ledger edge. That
edge was admitted before stop; sibling consumer ACK is explicitly modeled to
retain it for the real final engine. This is not complete recipient wake/IRQ or
hardware source refinement. The genuine global guard and Current exec callers
remain, and original masked caller scopes are still required.

#### Original masked exec retry / Current wake entries

Both Current exec loops now create ONE original `DispatchPreemption` per retry,
mask IRQs and lend it to the shared census/terminal engine. The original intent
and physically fenced census remain outside the reschedulable barrier; the CPU
loan releases BEFORE cancellation, external ACK and `cond_resched`. Deleted the
production `Scheduler::quiesce_captured_exec` adapter, not renamed or cfg-hidden.
Its isolated native catalog projection remains explicitly separate from the
all-build actual resource engine.

Current IPC `wake_task` likewise enters original masked CPU custody without a
Scheduler acquisition. Directory hint/descriptor scans produce candidates only;
actual BindingPin/backing retains exact UID through admission/effects. Original
Resource53 FrameReader exclusion owns final nonexecuting claim/dereference;
Current/Transition paths never reread a consumed frame. Exact return/wake CAS
arbitration covers either outgoing Blocked publication order. Terminal closes
fresh source loans before borrower drain and original Class55 census fences
physical member reuse, including the final process-wide donation cleanup.
Linux recipient capture remains a separate exact lifetime loan. Legacy unsafe
adapters still require real original masked CPU custody; a counter snapshot or
copied Current does not supply it.

Durable remote IPI hints flush AFTER all leaves release and BEFORE this CPU's IF
is restored. IRQ/preemption borrow, resource pins, frame reader and original
census jointly replace the old directory serializer for these entries only.
Actual old retry/wake witnesses now use the original borrowed CPU owner. Three
additional witnesses refuse IRQ-enabled or foreign-CPU scopes before effects
and verify stale directory hints cannot grant a UID. Caller IF/topology and
external ACK remain explicit native models; the IRQ-enabled constructor lends
an actual CPU cell, not a forged mask flag. Five mutation obligations bind the
scope/candidate gates. Selection/postclaim policy, remaining Current/control/IRQ
and cold Scheduler callers, boot/pager and full joint §2.7 refinement remain.
The genuine `cpu_local.rs::SCHEDULER` is NOT deleted; no hardware acceptance or
whole-goal completion is claimed.

#### Exact reply object custody versus live execution

Normal/cancelled reply scalar settlement no longer falls back to Scheduler on
missing, stale or mismatched hints. The resolver joins original Identity51 UID
and lifecycle to one actual Resources53 binding/start/context comparison. Live
state travels from that SAME writer, so the resource leaf never recursively
enters Identity51 or reverses lock order. The callback returns only scalar
object-match/live classification, not execution, MM, frame or budget custody.
A stale hint is only a candidate; complete bounded resolution cannot accept a
foreign UID, withdrawn original metadata or an unbound physical object.

Preserved the original settlement rule: a retained EXACT scheduling-context
object can still return reply custody after terminal stop. That does not make
its owner live and cannot authorize a wake or new charge. A live mismatched
object remains corruption; an absent old object is allowed only by cancelled
settlement. Donation release and subsequent wake keep their own transactions.
Fast reply handoff is now integrated into the original typed reply owner below; other enclosing legacy donation adapters remain separate and real.

Three actual born-object witnesses cover exact and foreign identity, original
metadata withdrawal, stale/missing hints and real terminal retained-object
classification. Native negative metadata withdrawal and external consumer ACK
are explicit models, not hardware or execution grants. Five mutations bind
writer/resource UID, full object, terminal live output and candidate fallback.
The genuine global guard and full joint deletion requirements remain open.

#### Original physical donation participants / reservation funding source

Six Current entries (effective System query, ordinary call admission, explicit
reservation, receive inheritance, reserved bind and pager fault inheritance)
now use `donation_publication`, with no scalar Current shortcut or Scheduler
fallback, including non-Current donors. One original masked same-CPU owner lends
entry custody; Terminal50 -> Identity51 -> Resource53 captures actual non-Copy
donor/root/receiver pins before the ledger leaf. No leaf crosses capture, queue,
external ACK or hardware work. Suspended tasks may retain donation metadata;
retired/exec-sealed/foreign bindings cannot capture new loans. These pins own
metadata/backing, never a new execution/MM/funding interval.

A private funding basis is only a candidate until the original physical loans
are retained and the ledger revalidates its exact source reply/namespace, donor,
receiver and root identities. New reservation/upsert also revalidates that the
same source is still the donor's current funding basis. Each new reservation
stores its original qualified source: a newer incoming reply may change the
worker's current source without changing that earlier reservation. Bind retains
that original source; root cancellation or inactive intermediate custody denies
bind even when the reservation row still exists. Failed bind does not fall back
to fresh self funding. A racing reservation/worker cannot be overwritten by a
fresh IPC upsert. Pager/reply numeric collisions remain separate namespaces.

The actual old scalar Current shortcut bodies and six synthetic publication-only
tests are deleted, not cfg-hidden. Their donor/receiver UID and trusted-class
mutants now refine actual physical loan effects; former Current-only deferral is
intentionally replaced by total exact resolution. Nine actual born-resource
witnesses cover those gates, loan counts, terminal seal ordering, capture/commit
source cancellation/change, retained original funding across newest-source
changes, deep inactive source refusal and distinct pager/reply namespaces.
Native CPU/topology and cleanup ACK remain explicit models. Legacy worker
selection/handoff and other remaining Scheduler adapters are still real; this
is not a joint dispatch/interval/hardware proof or complete guard deletion.

#### Original normal reply wake/FIFO lifetime and Current self-demotion

Current normal reply now independently withdraws its exact reply donation,
resolves a candidate and borrows one original wake Binding under a masked
same-CPU DispatchPreemption. The successful wake returns that SAME non-Copy
Binding through post-wake generation-token capture and both FIFO owner checks;
there is no lookup or reacquisition after wake. A raced terminal seal may remove
execution permission but cannot drain/reuse its backing before the original
loan drops. An invalid saved frame consumes the original loan before handing
the same terminal producer to borrower drain: generic wake preserves quarantine
success, while reply cannot manufacture a FIFO token from that terminal result.
Remote durable reschedules flush outside leaves before the original IF restore.

Self-demotion independently borrows the actual original physical Current bank
and exact Identity/Resource binding under the original masked CPU. Scalar
Current is only a candidate; missing/sealed identities, absent physical bank,
kernel context and foreign/unmasked scope refuse. Base System admission clears
and fair weight is capped monotonically; original reply effective class/funding
is untouched. The actual legacy demotion method and synthetic catalog witness
are deleted, not cfg-hidden. Six actual born-resource witnesses cover both
boundaries, exact FIFO mutation/generation rejection, original loan counts and
seal/reuse exclusion, independent equal-number pager funding, failed-wake
withdrawal and monotonic class cap. Native entry/topology/cleanup ACK are models;
FIFO length is a read-only actual-engine witness, never a grant.

IRQ rejected fast-reply fallback borrows the SAME original CpuDispatchTurn
preemption scope through `reply_scoped`; it never reenters a turn or calls the
catalog normal-reply adapter. That actual legacy adapter and its synthetic
one-Scheduler-operation witness are deleted. Worker handoff, fast settlement and
complete dispatch remain separate owners. Existing guarded handoff model
assumptions remain explicitly incomplete joint refinements: these component
witnesses do not prove the original one-Scheduler transaction or whole guard
deletion.

#### Original typed pager/service wait publication

Pager reply and process-service wake now use the actual independent wake engine;
the legacy reason-check/unconditional-wake methods are deleted. Wait58 owns
payload/arm publication under one per-slot leaf. A non-Copy `TypedWake` borrows
the ORIGINAL physical Binding and reserves one checked epoch through exact
clear, frame/queue publication and the last FIFO effect. Later arms, including
identical payloads, refuse until release. No RunOwner generation bump substitutes
for wait lifetime; no raw leaf crosses frame reads, queue locks or hardware work.
Cancelled epochs and wrong keys/kinds/processes refuse. Failed Running admission
restores the original payload inside the same leaf; exhausted epoch admission
refuses before pending/payload effects. Terminal cannot reopen or reuse the
original backing while the Binding survives. Pager and reply funding remain
distinct namespaces; withdrawal still precedes a failed wake.

Actual born-resource witnesses include a two-thread arm/capture race, same-key
rearm exclusion, cancelled match, terminal retention, Current/Blocked return,
nonexecuting reader/queue/FIFO, exact service process/scope and exhausted input.
Native earlier claim, IRQ/topology, negative nonce recovery and external ACK are
explicit models, NOT architectural return or the complete source-refining joint
§2.7 proof. `cpu_local.rs::SCHEDULER` and the production dispatcher remain genuine
owners; this transfer does not authorize guard deletion or QEMU.

#### Original claim → executing interval → architecture/return (dispatcher WIP)

A successful `FundedFrame::claim` now moves its ORIGINAL funding directly into
execution after frame CAS, allocation-bank transfer and original turn receipt
publication, before it returns. There is no CPU preparation mailbox and no
legacy Scheduler opener/cancellation body. Both ordinary/fast/fallback actual
claims use the same implementation. The claim-owned Running generation and UID
are not re-resolved from a scalar descriptor; retained allocation/execution banks
and the exact Context/Domain FundingPin supply lifetime through the last effect.
The non-migratable CPU clock baseline is captured by that original claim.

Architecture admission and final Current publication borrow the same original
turn/receipt and require its still-active interval. Settlement cannot authorize
architecture even if frame banks remain. Control-only return neither cancels
nor settles another interval; failed preclaim grants drop directly outside CPU
leaves. Cold initial Running admission is explicitly distinct and can never
capture donated funding. Its enclosing boot authority remains unfinished.

Physical root witnesses and the preserved/new mutation obligations qualify this
join, including later scalar charge publication, terminal settlement and unused
closing-funder cancellation. Native ACK is still modeled, not register/RSP
acceptance. The legacy dispatch/fast-IPC execution and idle-steal/active-balance
BODIES are now deleted. Actual IRQ calls independent selection/claim/interval/
return after releasing its raw directory. One original turn retains priority
Choice through actual Current/Transition publication; activation publishers
borrow their original masked CPU scope through the last FIFO/count/pending
publication. First-turn claim precedes head/count removal, so reader/funder
refusal cannot erase an earlier prefix. Exact migration retains the original
physical reader across intermediate Local->Migrating CAS and target mailbox;
its native witness checks the intermediate target, not a later repaired record.
Actual immediate calls publish the sole Direct FIFO; reverse replies publish
exact typed fair-owner/FIFO custody (or acknowledge the original executing Current
wake). The scalar peer is only a hint, never a substitute record or lifetime owner.

Born-resource native witnesses cover these actual core effects, while earlier
IRQ/topology and register/RSP ACK remain explicit models. The preemption model
now distinguishes raw-directory release, original priority choice, publication
and RSP completion; it is NOT a full frame/funding/MM/terminal/reuse joint
refinement. The five IRQ entry/profile/SIMD enclosures still use genuine legacy
authority. Complete class/funding/arbitration, those consumers, and joint §2.7
evidence remain required before actual global deletion or QEMU; component
evidence does not certify that larger boundary.

#### Original whole fast-IPC producer ownership

The legacy call commit, fast reply completion/settlement, scalar token-mint
methods and their entire `wake_handoff` module are deleted, not hidden. All
actual software IRQ and Current fast producers use `ipc_publication` after raw
directory release, borrowing the SAME original masked CPU scope. Original
CurrentWait, physical donor/root/receiver loans and typed pending wait survive
the final sender commit; reverse WakeWon cannot be overwritten by scalar block.
The donation ledger borrows these SAME participants, never replacements.

Original receiver binding and stack pin/reader own validation and frame bytes.
Priority publication waits outside raw leaves. Identity51 -> Affinity54 ->
FIFO56 -> Wait58 joins exact live UID/admission, bounded FIFO capacity, original
owner CAS and original typed wait clear. Refusal advances neither wait nor owner;
no newer CAS observation can be substituted. All leaves end before original
frame-generation publication, remote mailbox and deferred notification, with the
SAME reader retained through those effects. Invalid frames transfer an original
stop producer to durable quarantine; no borrower drains itself synchronously.

Reverse replies withdraw their exact donation namespace and use original typed
EndpointReply admission, fair owner and exact FIFO generation. A runnable actual
Current bank is acknowledged without minting an executing-frame token. Foreign
context or wrong wait cannot trigger a generic wake fallback. FIFO values are
not backing/funding/execution grants; the dispatcher still must acquire its
original physical claim and interval. The component model now separates reader,
loans, bounded owner/wait/FIFO join, remote mailbox, reverse wake and sender
commit. Native witnesses exercise actual born-resource production calls; earlier
IRQ/CPU topology/Current and capacity inputs remain explicit models. Neither
those witnesses nor this IPC component model establish the required full joint
MM/architecture/terminal/reuse or hardware refinement.

#### Original whole independent IRQ enclosure

All five actual timer/RTC/reschedule/software/saved-user entries now open
`cpu_local::irq_entry::DispatchEntry`, which owns ONLY the original masked CPU
turn and its selected original receipt. The entire SchedulerAccessGuard dispatch
helper implementation, legacy SIMD-save method and legacy profile entry/drain
methods are physically DELETED, not copied or hidden under cfg. The genuine
cold guard/global and Scheduler authority remain elsewhere.

Original Current's physical allocation bank qualifies control probes and SIMD
bytes. Software IPC saves its original SIMD BEFORE producer intent; subsequent
fast/fallback dispatch borrows the same turn and does not overwrite that save.
The actual producer borrows this original mask scope, consumes the sole FIFO,
and opens its exact original successful claim/interval. Architecture installation
and final selection authenticate that SAME still-active claim; the selected slot
comes from its receipt, never scratch. Drop publishes original Current/Transition,
then notifications outside leaves, then parks the SAME CPU scope until incoming
RSP. Own deferred-wake take borrows the mask scope, not a scalar foreign CPU.

Independent profile collection consumes original counter windows. Each entering
CPU's physical Current and UID execution permit are compared BEFORE shared epoch
admission, while identity/lifecycle publication is compared under the original
Identity writer. This diagnostic is explicitly NOT the former global-table sweep,
a lifetime grant, or proof of agreement for periodic-continuation CPUs. Old cold
sweep helpers still exist. Consumed report projection retains the original schema,
window/ranking and pending-report witnesses without lending a Scheduler table.

Five new born-resource native witnesses include real producer -> Direct FIFO ->
claim/funding/interval -> receipt-selected changed return with BOTH physical banks
retained, unchanged control and unmasked refusal. Privileged register installation,
CPU bank publication and assembly RSP ACK are explicit native modeled edges; the
physical SIMD save is exercised, but these traces do NOT certify CR3/MSR/RSP
hardware, the complete class/funding/final-CAS join, or joint MM/terminal/reuse.
Initial scratch corruption occurred after acceptance and let its mutant survive;
the strengthened witness corrupts before acceptance. A redundant early-mask
assertion mutant survived the original turn's independent refusal; the new witness
instead kills forbidden silent remasking. Fresh PS853, signed check/kernel,
source/contracts and FULL PR gate pass; original implementation1263 retained plus
six new mutations gives1269/1269 kills, original spec242/242 retained. The real
Scheduler/global deletion and complete §2.7 source-refining joint acceptance
remain outstanding.

The entire fault-worker/latency-floor/resumed exception architecture authority
now has independent production custody; its Scheduler bodies are deleted and no
actual IRQ entry acquires the old global. Exception ingress retains its ORIGINAL
committed CurrentWait through fixed worker publication and latency accounting,
then drops that loan and its dispatch scope BEFORE the blocking trap. Worker
precommit/Current wake borrows the exact PagerService epoch, never consumes an
assembly-owned frame or falls back to clearing another wait. Blocked worker
publication shares the actual original physical receiver reader and bounded
Identity51/Affinity54/FIFO56/Wait58 join with IPC; SAME reader, phase and binding
loans retain the last floor effect, outside every raw leaf. Durable funding still
belongs to the pager waiter syscall; ingress mutates no donation ledger.

Resumed architecture borrows the now-actual Current allocation and its exact
retained context MM execution pin through CR3/TSS/syscall-stack/FS/GS installation.
It manufactures no frame, activation or execution claim, and no scalar root
replaces the original loan. New native born-resource and original mutation
witnesses cover worker capacity/epoch/precommit, reader lifetime through floor,
remote hint versus execution grant, and retained MM capture after terminal
admission closes. Actual exception trap, privileged registers and incoming RSP
are not exercised by these host witnesses; source ordering and component models
are not the still-outstanding source-refining joint/hardware acceptance.

#### Joint original class/funding admission through the actual owner CAS

Production selection now preserves its original class observation through the
same non-Copy Frame/Funding claim; it never repairs a selected peer with fresh
class/funding. Actual Local rq40 precedes ledger45, original Context48/Domain49,
then Terminal/Identity/Resources/Placement and exact CAS. Live base replacement
uses the same ledger publisher. All leaves end before banks, interval or
architecture effects. Original refused MM/UID loans return in non-Copy
ClaimAttempt and Local drops rq before loan Drop. Current fallback shares the
original selection observation; fixed idle retains physical kernel/UID/placement
custody but is not a policy-class/funding winner. The old funding precheck and
unused guarded claimable fallback bodies are deleted, not renamed.

Seven new native witnesses, PS866, signed kernel and complete PR lanes pass;
FrameReaderClaim now joins class/original funding/final CAS (65720 states), with
all original mutation witnesses retained. Native mutex probes and earlier IRQ/
RSP inputs are not hardware evidence; this bounded model excludes complete
Current/Transition/MM/settlement/terminal/reuse. Those joint boundaries, whole
cold IPC/group/signal/boot consumers and actual Scheduler/global deletion remain.

#### Obsolete process-donation authority deleted

The arbitrary runnable process-worker reply-binding API and guarded process
PID cancellation scan, their Scheduler bodies and public reexports are deleted.
Neither had a production caller; actual IPC publication already preserves
AwaitingReceiver until the exact IPC_RECV worker. Ready hints never bind caller
budget to a worker serving another request. Production terminal publication
retains the original physical process member census through donation withdrawal
and final consumers; it never replaces membership lifetime with a fresh PID
scan. The redundant native retirement scan is also removed, not copied or
hidden. New actual born-process witnesses cover awaiting exact receive and
whole member withdrawal/final publication/backing ACK; external ACK/earlier VA
remain native models. Complete joint/global deletion and hardware acceptance
remain open; the actual cold producer is independently transferred below.

#### Original cold IPC producer before sender wait arm

The actual compat enqueue now passes its exact endpoint and parked receiver
into `ipc_publication::call_before_arm`. ENTIRE Scheduler fused transaction and
standalone synchronous-hint authority/API/reexports are deleted, not renamed,
copied or cfg-hidden. Original masked Current bank and donor/root/receiver loans
survive a SINGLE typed receive pending phase: funding admission precedes any
runnable effect, and original phase/frame reader survive queue and final FIFO.
Unlike the IRQ hot call, this producer never arms or commits the sender; later
reply observation/arm/commit still arbitrates WakeWon. Full bounded urgency
FIFO may refuse a hint without undoing successful fair wake. A generic hint is
urgency only; later selection must independently own its physical frame/claim.
Raw or overlapping callers refuse BEFORE IPC reservation/publication; later
exact typed refusal cancels the published reply unless runtime proves returned
custody. Corruption transfers the original stop producer to durable quarantine,
never drains the caller's still-owned physical/funding loans synchronously.
The sole physical wake engine is shared, not copied. Four born-resource
witnesses and the extended bounded hot/cold IpcFastHandoff model refine these
receiver effects/unarmed sender. Topology/IRQ/loader remain modeled inputs;
this does NOT complete Current/Transition/MM/interval/return/terminal/reuse joint
refinement or permit QEMU before actual complete legacy deletion.

#### Independent BSP/AP initial execution family

All legacy reset/secondary-idle/initial-architecture bodies and the entire
`architecture.rs` are deleted; actual Bootstrap owns no Scheduler instance.
Original InitialCpu admits a shared sole hidden kernel descriptor Ticket,
pre-ACK physical backing, exact CPU Running/frame generation and typed initial
interval. Original producer backing overlaps both physical/UID Current banks
and BSP SIMD save. Global owner initialization first refuses any live resource,
writer, pin or interval reference: not teardown/reset of an earlier lifetime.
BSP uses original reserved FrameWriter and physical Current frame pin/reader
before validation. AP writes only guard words on its exact generation-qualified
parked HAL static stack and retains live HAL RSP; no synthetic saved AP frame or
cross-CPU local-clock start. Hardware installation borrows SAME original image/
entry HardwareScope and masked CpuDispatchTurn, both retained banks and active
original interval through the last register. The kernel global root namespace
is not a newly minted user-MM activation grant. Initial banks precede logical
Current, every CPU ACK and BSP architecture precede Ready, and exact AP entry
architecture precedes Online. Partial images poison rather than force ACK/reuse.

Born descriptor/interval/bank/native architecture-capture witnesses and extended
CpuOnlineLifecycle bounded initial prefix retain original mutation obligations.
Earlier HAL/static AP stack/logical Current inputs are explicitly modeled;
privileged installation and actual RSP are NOT native acceptance. This does not
complete selected funding/MM/Current/Transition/return/terminal/reuse joint or
permit QEMU before complete production legacy/global deletion.

#### Independent signal and complete process job-control family

All six legacy signal/SIGCHLD/recipient/stop/continue/transfer bodies and the
PID-only stop/continue event producers are deleted. Real Current entries use
an original recipient BindingPin through Linux metadata and final typed wake;
SIGCONT/KILL retain that same pin through complete group resume before signal
publication. Process Change owns the original process pin and membership census;
a purpose-qualified Cohort retains every exact member UID through ALL gates,
reschedule requests, exact process event and parent SIGCHLD before one admission
release. Job control cannot publish affinity masks, terminal members never reopen,
and post-stop creation refuses. Raw leaves end before external wake work. Stop
borrows the SAME original Current physical/MM/UID/interval until the last effect.
Untouched contention returns a retry: broker delivery reports EAGAIN, and Current
stop retains the pending cause until a committed transaction, never consumes it
on refusal. No service policy moved into ring0.

Born-resource regressions and bounded SigchldNotification job-control refinement
cover retained units, partial admission, coalescing and terminal reuse. Earlier
CPU/topology and final external ACK remain modeled inputs. These component
witnesses are NOT full selection/claim/MM/Current/Transition/execution/return/
terminal/reuse joint refinement or privileged hardware acceptance. Genuine
Scheduler/global deletion and all post-deletion acceptance remain outstanding.

#### Original cold Current observations and final cache effects

The eight legacy runtime/target-observation/TLS/root-idle/alternate-stack methods
are deleted with their actual production guard callers. Runtime snapshot borrows
SAME OriginalCurrent physical/MM/UID and non-Copy active interval, including its
original FundingPin through the last paired Context/Domain read. Descriptive keys
are copied under Interval52, then that raw leaf ends BEFORE Context48 -> Domain49;
no fresh donor lookup/admission, scalar key or generation check replaces lifetime.
Closing or donation metadata replacement cannot redirect this read. Original
interval loan still excludes settlement and reuse. TLS cache requires the same
Linux cell's already accepted value; root-idle requires original kernel root
Current and active interval. Target snapshots retain an original exact UID across
context/start/Linux cell, and new admission refuses after close. Accelerator
absence/collision is not authoritative absence: target snapshots, signals and
terminal requests qualify original loans over bounded full candidates. Scalar
waiter-registry liveness instead linearizes under the exact identity writer;
transient exec/pin refusal and pending close are not committed task death. This
observation grants no lifetime or execution authority.

The unconsumed public `CurrentKernelStackScope` and scalar previous-bounds restore
API are removed entirely, not renamed or cfg-hidden. There are no in-tree users;
future alternate-stack callers need original backing custody through their last
effect. Born Current/root/target, original donor and exact timeout-pair regressions
plus the bounded original-funding observation extension distinguish copied donor
metadata from retained funding. Earlier CPU/ACK inputs remain modeled. Full joint
selection/MM/architecture/return/terminal/reuse and real hardware are NOT certified.

#### Original wake, ordering and activation publication

The complete legacy hint/wake/process-worker/donation/activation producer family
is deleted, including the native Scheduler activation implementation. Actual
Current APIs use `ordering_publication`, user-qualified `wake_publication` and
original `Activation::commit_owned`. Hints retain original target BindingPin
through qualification, FIFO/floor and notification request; process selection
retains its winning original target, never a copied slot reacquisition. Publication
arbitrates with CPU choice before policy leaves. Optional floor borrows SAME
OriginalCurrent physical/MM/UID/interval through the last effect, excludes self
and idle callers, takes the tighter caller/strict-band floor and never raises
vruntime. Missing Current permits ordering only, not donation or funding transfer.
Deduplicated spawn cannot donate twice; latency FIFO is bounded, User-qualified
and does not wake blocked work. Generic hints now carry exact UID, rechecked
before class/Local admission. This is still only ordering, not an execution grant.
User-only wake keeps the same original user binding through the existing wake
protocol; kernel identity cannot clear a wait. The activation entrances share
bounded input preflight, while actual Cohort owns participant pins and admission.
The complete native activation callback/capacity witnesses now use actual born
resources and production publication instead of a second Scheduler algorithm.

BootstrapActivationHandoff adds original target capture/publication/drop/reuse
and exact-UID consumption. Its new mutation witnesses and born-resource tests
are component refinement only: earlier wait/class/topology/Current inputs and
external completion remain native projections. They do NOT close the complete
selection/claim/Current/Transition/MM/interval/return/terminal/reuse joint or
hardware boundary. Actual global deletion and all post-deletion acceptance are
still outstanding.

### 2.7 Guard deletion requirements

The actual global Scheduler guard and all remaining Scheduler bodies/type are
physically deleted. Historical witness/formal porting and the complete
source-refining joint deletion audit now pass (current handoff records evidence).
Earlier guarded component proofs do not certify the new boundary. This accepts
SOURCE deletion, not hardware behavior, performance or final goal completion.
The independent owners must continue to satisfy ALL complete boundaries below:

1. **Frame/stack/SIMD and switch lifetime:** independently pin exact task/frame
   generations through validation and switch; publish outgoing Transition and
   incoming Current custody before any reclaim can observe absence. A seqlock
   recheck after an unsafe frame read is not a lifetime pin. Stack banks and
   exact context MM pins now retain backing through commit (§2.6b), including
   old exec shared backing. Same-turn reader/claim/architecture arbitration is
   bound by the joint source checks and original-bank concurrency witnesses.
2. **Execution interval and donation settlement:** consume each same-CPU
   interval once, bind its actual funding identity/reply through charge and
   return/cancellation, and prevent context/domain reuse before settlement.
   The CPU bank and funding pins now own executing intervals (§2.6b); complete
   CPU return now borrows the original successful claim and validates its exact
   still-active interval; failed preclaim funding drops directly outside leaves,
   with no scalar-keyed CPU preparation/cancellation authority. Same Foreign
   grant/claim/switch/settlement refinement retains original funding across later
   donation metadata. Context→Domain mutexes alone are not interval lifetime.
3. **Cross-owner lifecycle transactions:** identity/lifecycle writer custody
   and four gates have an independent owner (§2.6b). Admission, activation,
   affinity/class changes, exec and retirement retain independently owned
   context/frame resources and exact publication ordering.
   Preserve partial initialization, failed spawn, cancellation and terminal
   slot-reuse rules; a leaf writer mutex alone does not transfer these lifetimes.
4. **Dispatch/context metadata:** production facade data is now removed;
   current/timing/profile and boot/idle state have independent owners. Complete
   cold transactions and call sites now use original Ticket/image, binding and
   Current loans, not guarded facade access. Telemetry is never dispatch/billing authority.
5. **Teardown:** independent cleanup claim/ACK and exact retired detachment now
   retain terminal/resource custody through kernel-owned external completion.
   Enclosing terminal producers, unpublished rollback and process/MM/reuse
   retain independent original seals; the same joint trace reaches actual reaper
   and a fresh constructor/terminal cycle. Hardware effects remain a separate
   runtime gate. No owner lock crosses external work.

Only then delete production Scheduler authority and its hot-path guard,
replace the legacy guarded formal assumptions with source-refining owner
models, and validate the resulting kernel. Keep IRQ/preemption, lock order,
blocking/allocation, fault handling and reference publication/reclaim evidence
for each applicable boundary. No policy moves back from named userspace
services as a compatibility or performance shortcut.

Current user constraint: **no QEMU until safe complete legacy deletion; no
scoped commit before successful QEMU.** Preserve unrelated changes and signing
hooks. After deletion, independently validate 1/2/4/8-vCPU custody/fairness and
requested workloads, then measure dispatch cost, contention and tail latency.
Old readiness failures stay failures; no controlled scheduler speedup or full
production-path equivalence has been established by the current host/proof
fixtures. Do not silently attribute UI/VMA failures to this scheduler migration.

Execution intervals may use `scheduler_local_nanos()` only for one
non-migratable same-CPU start/end pair, never cross-CPU ordering. The RTC tick
baseline remains the fallback; publish/reclaim clears local baselines before
reuse and preserves the minimum-charge fairness policy.

## 3. vfsd receive owner and bulk lane (`V5-VFSD-HOL-007`)

QNX's rule is the whole design: a server should always have a RECEIVE-blocked
thread, and a kernel boost that compensates for the absence of one is working
around a badly built server. vfsd currently has one serve loop that performs
snapshot reads, which is exactly the shape QNX names.

Four owners:

- `VfsEndpointReceiver` — always receive-capable. Validates the envelope and
  produces an immutable `RequestTicket`. Performs no I/O.
- `VfsNamespaceOwner` — takes the namespace state briefly to resolve a request
  into an immutable `SnapshotPlan { mount_gen, inode_gen, extents,
  expected_hash, reply_cap, deadline }`.
- `SnapshotWorkerPool` — bounded workers execute a plan holding no mutable
  global state.
- `VfsCompletionOwner` — re-takes the state briefly, revalidates the
  generations against the plan, commits the cache entry, and replies.

Control and bulk are separate queues with separate capacity and budget, so a
namespace-only request never queues behind a bulk read. `sched_yield` is not a
progress mechanism and must not appear in this path. The absolute deadline comes
from loaderd (section 6) and is checked at the completion owner, not per phase.

Two hazards already recorded and still binding: `lock_vfs_storage()` must be
acquired once per call — two live guards in one expression spin forever — and
the read must stay a single read, because 64 KiB chunking makes FAT re-walk the
cluster chain per chunk and defeats `should_materialize_file_cache`.

Probe: enqueue, receive, plan, first block, last block, seal, reply timestamps
on one ticket id. Acceptance: control-lane queue residence p99 under 2 ms while
a snapshot is in flight.

## 4. uiserver protocol, scene, and submission owners (`V5-WAYLAND-HOL-013`, `V5-GPU-UI-OWNER-014`)

### 4.1 What the protocol actually requires

From the Wayland protocol specification —
<https://wayland.freedesktop.org/docs/html/apa.html>:

- `wl_buffer.release` is "sent when this wl_buffer is no longer used by the
  compositor", and it **may legitimately arrive before** the frame callback of
  the commit that attached it, in which case "the client is immediately free to
  reuse the buffer and its backing storage, and does not need a second buffer".
- The frame callback should be timed so the server "give[s] some time for the
  client to draw and commit after sending the frame callback events to let it
  hit the next output refresh", and should be withheld when the surface is not
  visible.
- On commit, "the wl_buffer is applied before all other state", and content
  updates with a dependency graph "must be applied atomically".

This corrects the audit's design in one specific way. The audit models a single
`FramePresented` completion returning to the protocol owner. That is not
sufficient: **release and presented are two different facts with different
timing**, and collapsing them either releases a buffer the scanout still reads
or withholds a release the client is entitled to early. The GPU owner therefore
returns two distinct completions.

### 4.2 The three owners

1. `WaylandProtocolOwner` — sole owner of the backend poll fd, the client list,
   object lifetime, dispatch, flush, and rearm. It is the only sender of
   `wl_buffer.release` and `wl_callback.done`, and it sends each **only on the
   matching completion**, never on submit.
2. `SceneOwner` — converts protocol events into an immutable `SceneDelta`.
   Because commit is atomic, one commit produces exactly one delta; a partially
   applied delta is a protocol violation, not a performance trade.
3. `GpuSubmissionOwner` — consumes an immutable
   `FramePlan { frame_seq, scene_gen, surface_gens, buffer_gens, damage, deadline }`
   and performs atlas preparation, copy, ioctl, fence wait, and present.

Completions, both carrying `frame_seq`:

- `BufferStorageReleased { frame_seq, buffer_gen }` — emitted as soon as the
  submission owner can prove the storage is no longer read, which may precede
  presentation.
- `FramePresented { frame_seq, scene_gen, transport_epoch, slot_gen, timestamp }`
  — or a terminal error.

Queue-full is handled by coalescing to the latest compatible scene or by an
explicit deferred frame. CPU full-scene fallback stays forbidden, and the last
valid front is retained under mandatory GPU. No lock is held across owners; all
crossings are bounded queues plus capability generations.

### 4.3 What the current measurement means for scope

Frame instrumentation over 3120 frames showed one slow present, one slow partial
present, and one slow loop, and the single blocking `retire_oldest(true)` is on
the activation frame only. So this split is **ownership hardening, not a frame
rate fix**, and it must be justified and gated that way. The frame records are
the before-and-after check; a regression in them rejects the split.

## 5. Frame identity across domains (`V5-UI-PIPELINE-011`)

`frame_seq` is minted by `SceneOwner` when it publishes a delta and travels
unchanged through `FramePlan`, both completions, and the protocol owner's
callback and release. To join the kernel side it is carried as an opaque
correlation id on the scheduler wake/run probe, the IPC enqueue/receive/reply
probe, and the DVM relay ACK, so one identifier spans wake to photon.

The attribution rule is the audit's and stays: if only wake-to-run exceeds,
suspect placement; only IPC residence, suspect a daemon receive owner; dispatch
to scene, suspect protocol ownership; scene to submit, suspect full-frame work;
submit to fence, suspect GPU or DVM; all on-CPU phases short but wall long,
suspect scheduling or VM exits.

**Prerequisite.** The milestone ring drops records under 8 vCPU load — 90 of 351
sequence numbers were missing from one run, and the drops land at the tail of the
per-second drain where the acquisition census is emitted. A trace that silently
loses records cannot support attribution, so bounded, loss-reported emission is
part of this item, not a separate cleanup.

## 6. One absolute deadline type (`V5-DEADLINE-012`)

```rust
pub struct AbsoluteDeadline { start_ns: u64, end_ns: u64 }

impl AbsoluteDeadline {
    pub fn remaining_ns(&self, now: u64) -> Option<u64>;
    pub fn child_timeout_ns(&self, now: u64, cap_ns: u64) -> Result<u64, DeadlineExpired>;
}
```

One type in a shared library, carried by inputd session mutation, loaderd
snapshot, vfsd bulk work, and the UI frame path. A transaction start fixes the
end; every child call uses `min(remaining, cap)`; a fixed sleep is
`min(remaining, backoff)`. Monotonic reads only — wall clock is never ordering
authority. Phase-local stopwatches are deleted, not wrapped.

This also removes the confusion that made a transient look terminal: a provider
that is not ready yet must return a distinguishable transient status rather than
`ENOSYS`, and a caller must fail fast on genuinely terminal errno values.

## 7. Transfer authority (`V5-IPC-AUTH-015`, `V5-FORMAL-IPC-018`) — closed

Both are closed; see the correction in section 1 for why they briefly appeared
open and for the model-to-source refinement map. Nothing is owed here beyond
keeping that map written down: the model and the source use different
vocabulary for the same binding, and the next name-based search will draw the
same wrong conclusion if the map is removed.

## 8. TLB targeting (`V5-TLB-SCALE-009`) — structure closed

Current source is better than the audit assumed, in two ways.

`flush_for_reclaim` already targets only CPUs whose published `ACTIVE_ROOT`
matches the mutated root, falling back to every eligible CPU for global scope.
Linux's cache and TLB contract —
<https://docs.kernel.org/core-api/cachetlb.html> — states the equivalent
optimization: "if it can be proven that a user address space has never executed
on a cpu (see `mm_cpumask()`), one need not perform a flush for this address
space on that cpu", with flushes ordered after page-table changes.

RustOS reaches the same conclusion by a different route, and that route is a
coupling rather than a coincidence: the tree enables **neither PCID nor
INVPCID**, so a CR3 write flushes non-global entries and a CPU that switched
away from a root provably retains none of its translations. With PCID enabled,
translations survive the switch under their ASID; a CPU that merely *ran* the
address space still holds them; and targeting by *currently active* root would
skip exactly those CPUs and authorize reclaim of frames they can still
translate. That is memory corruption, not a latency regression.

`active_root_targeting_is_sound` now asserts CR4.PCIDE is clear, checked per
admitted CPU rather than once because PCIDE is a per-CPU control register and a
BSP-only check would not cover an AP that came up differently. A mutant that
weakens the predicate fails. Enabling PCID must therefore come with an ASID
allocation, reuse, and wrap proof and a different target set, per Intel SDM
Vol. 3 — <https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html>.

The unacknowledged-target policy was already settled in source and did not need
choosing: the module documents **system fail-stop**, not CPU quarantine, and
records the rejected alternative — letting the deadline expire into reclaim —
as memory corruption rather than a latency problem, so no timeout on this path
may authorize reuse.

What remains is runtime evidence only: the ACK latency distribution and reclaim
sequencing under a deliberately delayed vCPU, with zero reclaim-before-ACK.

## 9. Lazy stack growth (`V5-MM-STACK-DEAD-021`) — closed

There is no dead route to delete. The product maps every usable stack page
eagerly above one permanent guard page, and that is enforced rather than
documented: a `const` block asserts
`USER_STACK_INITIAL_COMMIT_PAGES + USER_STACK_GUARD_PAGES == USER_STACK_RESERVE_PAGES`,
so lowering the commit fails the build, and no page-fault path grows a stack.
`release_stack_maps_every_usable_page_above_one_guard` pins
`reserve_start == committed_start`.

The reason the eager map exists is recorded where it will be read: exception
context cannot wait for `ProcessStateLock`, because another thread may hold it
when a valid growth fault arrives. If lazy growth is ever reintroduced it needs
a deferred fault worker first, and it must distinguish lock contention from an
invalid address — merging the two is what turns contention into a spurious
fault.

## 10. Order of work

Ownership first, then the measurement that proves it, then tuning. Nothing in
this document is a tuning change.

1. Section 6, the deadline type, because sections 3 and 4 both consume it.
2. Section 3, vfsd lanes — the current runtime blocker.
3. Section 5's loss-reported trace, since sections 2 and 4 are unverifiable
   without it.
4. Section 2, the per-CPU scheduler, staged exactly as 2.7 describes.
5. Section 4, the uiserver split, gated on the frame records not regressing.

Sections 6, 7, 8, and 9 are done. Only sections 2 and 4 remain, and both are
staged migrations rather than single changes: landing either without the
validation each describes would repeat the per-slot owner-word failure, which
passed every unit test and only broke in KVM.

A per-item claim of closure requires the construct to exist in source *and* a
mutant that fails without it. Source inspection alone is what produced the
matrix this document had to correct.

## 11. Process identity publication

The global `ProcessTable` remains the exact lifecycle authority, but ordinary
user-copy and IPC checks no longer acquire it merely to revalidate the current
process. Each slot publishes one committed word containing the process-slot
generation and MM generation, alongside its PID and stable state pointer.

Lifecycle writers revoke the committed word before changing payload and
release-store the new exact identity last. Readers accept only a stable
word/payload/word observation. Missing, revoked, stale, or torn publication
falls back to the locked table, so the optimization does not weaken the
authority decision. The runtime-profile drain compares every published slot
with the table outside the scheduler guard and reports divergence without
placing diagnostics on the hot path.

### Strict early class decisions on incomplete observations

Production overdue-System and due-User reservation selection now use the same
qualified CPU-local queue/class observations in profile-on and ordinary
builds. A torn earlier policy observation retries up to four fresh epochs;
only a complete negative result permits later handoff/fair policy. If all
attempts remain incomplete, a qualified current Local generation or exact
published target idle generation may be retained; otherwise the guarded idle
fallback runs. No incomplete earlier class result licenses a catalog peer or
allows a later policy arm to overtake a possibly ready-aged System or reserved
User. Owner generation, saved frame and final CAS remain independently checked.
This bounded retry does not establish liveness under a permanently odd writer.
Production non-FIFO Local hints now require complete published admission at
consumption and fresh qualification of the selected owner generation through
final frame and claim; an incomplete or changed hint loses its one-shot order
but its Local fair queue retains custody. A Direct/RemoteQueued owner cannot
borrow catalog schedulability as a plain hint. The ordinary non-FIFO User hint's strict-System veto now also scans complete
published queue/class epochs and qualified System peers. Incomplete peer,
frame or epoch evidence vetoes only that hint, leaving fair custody intact;
no catalog comparator supplies positive hint authority. Consumed atomic
activation and ordinary spawn first turns now compare the record's task ID
with a complete published identity at admission and again at owner binding;
slot reuse or missing identity drops only one-shot priority. The latency exception is admitted with a stable published User class and
matching task ID, carries `{slot, task_id}` in its fixed FIFO, and rechecks
both identity and User class on consumption. Slot recycling replaces the old
record for that slot without growing capacity; missing/changed identity or a
torn class publication cannot spend that queue's budget. The lifecycle producer still
constructs first-turn records under the global guard, which continues to
serialize donation and teardown, so deletion remains unsafe.
