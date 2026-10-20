# Scheduler policy, measurements and ordering

[Contract index](../structural-ownership-design.md).

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

