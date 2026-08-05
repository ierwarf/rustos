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

So section 1 re-derives status from source. Everything after it is design for
what that check found still open.

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

### Open, and owned by this document

| item | what is actually missing |
|---|---|
| `V5-SCHED-GLOBAL-001` | no `PerCpuScheduler`, no backend selection; one `static SCHEDULER` still serializes every CPU. Section 2 |
| `V5-FORMAL-SCHED-019` | `SchedulerCpuOwnership.tla` models the guard, not the removal of the guard. Section 2.7 |
| `V5-VFSD-HOL-007` | `ExecutableSnapshotPlan` exists, but there is one receive loop, one shared storage lock, a single-slot worker, and no control/bulk lane split. Section 3 |
| `V5-WAYLAND-HOL-013`, `V5-GPU-UI-OWNER-014` | no `WaylandProtocolOwner`, `SceneOwner`, `GpuSubmissionOwner`, or `FramePlan`. Section 4 |
| `V5-UI-PIPELINE-011` | `frame_seq` exists only inside `uiserver/main.rs` and `loop_timing.rs`; it never reaches the scheduler, IPC, or DVM relay. Section 5 |
| `V5-DEADLINE-012` | `AbsoluteDeadline` exists only in `inputd/dvm_session_sync.rs`. loaderd, vfsd, and the UI frame path each keep their own phase-local timeouts. Section 6 |
| `V5-IPC-AUTH-015` | `IpcTransferTicketWire` binds `(transfer_id, nonce, batch_generation)`. There is no receiver-set epoch anywhere in `kernel/` or `libs/`. Section 7 |
| `V5-FORMAL-IPC-018` | **reopened.** `IpcTransferAuthority.tla` models `receiverSetEpoch`, and its binding row claims it binds descriptor authority. The implementation has no such field, so the model proves a property of a system that does not exist. Section 7 |
| `V5-TLB-SCALE-009` | targeting is sound but the argument is undocumented, and the quarantine-versus-fail-stop choice is still unmade. Section 8 |
| `V5-MM-STACK-DEAD-021` | latent only; the product maps stacks eagerly. Section 9 |

`V5-FORMAL-IPC-018` moving from closed to open is the reason section 1 exists.
A model that is more capable than its implementation is not neutral: it reports
green for an authority binding that no code performs.

## 2. Per-CPU scheduler (`V5-SCHED-GLOBAL-001`, `V5-FORMAL-SCHED-019`)

### 2.1 What the measurement changed about the target

Two sessions of measurement resized this item and the design has to reflect
that, not the audit's original framing.

- The audit assumed dispatch selection was the serialized resource. It is not:
  the guard is taken about eighteen times per dispatch, and the excess is wake,
  pick hints, donation, affinity, and lifecycle traffic.
- The largest callers were read-only identity queries, now answered from a
  published per-slot record, and the syscall SIMD pair, now held outside the
  lock entirely.
- What remains on the lock after that is genuine scheduling: at 1 vCPU the top
  caller is `irq.rs:712`, the voluntary-yield dispatch, and its worst owner turn
  is essentially fully attributed to in-owner segments.

So the target is not "make the critical section shorter". It is **CPU-local
authority**, so that the fourteen-odd acquisitions a dispatch needs stop being
global.

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
| `TaskRunAuthority.owner` | any CPU, by CAS only |
| `transfer` slot | the CPU that won the owner CAS, before publishing the mailbox entry |
| saved frame, kernel stack, FPU/SIMD, TLS, mm-active bit | the `Running` owner CPU only |
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
assertion already enforced in `SchedulerAccessGuard::drop` and must survive the
cutover unchanged.

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

### 2.7 Cutover, and the model that has to come with it

1. Compute the new state as a **shadow read-only** projection under the legacy
   backend and emit divergence markers only.
2. Select one authoritative backend at boot, `Legacy` or `PerCpu`. **Dual-write
   is forbidden**; the audit is right that a dual-authority route is how this
   kind of migration fails silently.
3. Enable in order: 1 vCPU, then 2 with remote wake and mailbox, then migration
   and steal, then donation and deadline at 4 and 8.
4. Only after acceptance, delete `context.ready` authority, the global ready
   scan, and the legacy formal model.

`V5-FORMAL-SCHED-019` closes with a refinement model whose variables are the
owner word, the per-CPU queues, the transfer token, `current`, and the
transition stack, and whose properties are exact-one ownership and a
queue-to-owner refinement. It must kill these mutants: guard removal without
owner-word protection, dual-write divergence between legacy and per-CPU state,
transfer token reuse, and swapping the source and destination halves of a
migration.

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

## 7. Receiver-set epoch (`V5-IPC-AUTH-015`, `V5-FORMAL-IPC-018`)

Today `IpcTransferTicketWire` binds `(transfer_id, nonce, batch_generation)`.
`IpcTransferAuthority.tla` binds a `receiverSetEpoch` that the implementation
does not have, so the model currently over-claims.

Design: a kernel channel registry keyed by the unix channel id owns a
`receiver_set_epoch`, incremented on every event that changes which open
descriptions may receive — `dup`, `fork`, `close` of a receiving description,
and service epoch revoke. The ticket binds source, service, channel, receiving
open-description generation, receiver-set epoch, and the accepted byte range. A
claim requires that the claiming receiver is a current member of that set and
that the epoch is unchanged. `TransferBatchAuthority` remains the terminal owner
and the first terminal CAS wins across install, reject, peer close, queue
discard, sender or receiver exit, and provider epoch revoke.

Mutants the model must kill after the change: dropping the epoch check,
dropping the membership check, allowing a duplicate commit, and dropping the
peer-close terminal.

## 8. TLB targeting residual (`V5-TLB-SCALE-009`)

Current source is better than the audit assumed. `flush_for_reclaim` targets
only CPUs whose published `ACTIVE_ROOT` matches the mutated root, or every
eligible CPU for global scope. Linux's cache and TLB contract —
<https://docs.kernel.org/core-api/cachetlb.html> — states the corresponding
optimization directly: "if it can be proven that a user address space has never
executed on a cpu (see `mm_cpumask()`), one need not perform a flush for this
address space on that cpu", and that flushes happen *after* page table changes.

RustOS gets the same property from a different fact: **the tree enables neither
PCID nor INVPCID**, so a CR3 write flushes non-global entries, and a CPU that
has switched away from a root retains no non-global translations for it. Active
root matching is therefore sound *because PCID is off*, and that is a coupling,
not a coincidence.

Three things close this item:

1. State the argument where it can be broken: an assertion, or a compile-time
   gate, that fails the build if PCID/INVPCID is enabled while active-root
   targeting is in use. Enabling PCID must come with an ASID allocation, reuse,
   and wrap proof, per Intel SDM Vol. 3 —
   <https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html>.
2. Make the unsupported-hot-remove policy one documented choice. A timeout must
   never permit a free; either the CPU is quarantined or the system fail-stops,
   and the contract names which.
3. Runtime evidence: ACK latency distribution and reclaim sequencing under a
   deliberately delayed vCPU, with zero reclaim-before-ACK.

## 9. Lazy stack growth (`V5-MM-STACK-DEAD-021`)

The product maps stacks eagerly, so the fault-growth route is unreachable. The
item closes by deleting the dead route rather than documenting it; if lazy
growth is ever reintroduced it must distinguish process-state lock contention
from an invalid address and use a bounded fault-retry continuation, because
merging the two is what turns contention into a spurious fault.

## 10. Order of work

Ownership first, then the measurement that proves it, then tuning. Nothing in
this document is a tuning change.

1. Section 6, the deadline type, because sections 3 and 4 both consume it.
2. Section 3, vfsd lanes — the current runtime blocker.
3. Section 7, receiver-set epoch, which also un-stales `FORMAL-IPC-018`.
4. Section 5's loss-reported trace, since sections 2 and 4 are unverifiable
   without it.
5. Section 2, the per-CPU scheduler, staged exactly as 2.7 describes.
6. Section 4, the uiserver split, gated on the frame records not regressing.
7. Section 8 and section 9.

A per-item claim of closure requires the construct to exist in source *and* a
mutant that fails without it. Source inspection alone is what produced the
matrix this document had to correct.
