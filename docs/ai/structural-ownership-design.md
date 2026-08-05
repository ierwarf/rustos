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
| `V5-SCHED-GLOBAL-001` | **narrower than the item text.** The per-CPU runqueue, owner words, remote mailboxes, and per-CPU selection all exist and are already lock-free. What is left is that the `Scheduler` struct's per-task arrays still sit behind one `TrackedSpinLock`, so the remaining work is a data-structure split. Stage one is done: the owner word and the legacy tables were proven to agree, zero mismatches at 1 and 8 vCPU. Section 2 |
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
per-task arrays — `contexts` (ready, blocked, vruntime, stacks, address-space
root), `starts`, `retired`, the SIMD slots, and the lifecycle flags.

**The remaining work for `V5-SCHED-GLOBAL-001` is therefore a data-structure
split, not a scheduling-algorithm change.** The per-task fields only the owning
CPU mutates have to leave the globally locked struct for per-slot storage whose
writer is that CPU, exactly as the writer table in 2.4 states. The scheduling
policy above them is already per-CPU and does not move.

This is also why `context.ready` matters more than it looks. It duplicates what
the owner word already says, and stage one proved they agree — zero mismatches
at 1 and 8 vCPU. Removing it as authority is the first field of the split, not a
cleanup.

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

Restated for what the tree actually is. The policy layer does not move; the
per-task state does.

1. **Prove the two authorities agree** while the global lock still serializes
   both. Done: `run_authority::compare` sweeps every slot once per drain and
   reports the first disagreement with a direction, and the calling CPU's
   in-flight dispatch pair is excluded because it disagrees by construction.
   Zero mismatches at 1 and 8 vCPU.
2. **Retire `context.ready` as authority.** It duplicates the owner word, which
   step one proved. Every reader moves to `runqueue::owner(slot)`; the field is
   deleted rather than left as a shadow, because a shadow is what goes stale.
3. **Move the remaining per-task fields out of the globally locked struct**, in
   the writer classes of 2.4: saved frame, kernel stack, FPU/SIMD, TLS, and the
   mm-active bit are written only by the `Running` owner, so they belong in
   per-slot storage that CPU owns. Lifecycle rows stay behind the directory
   token.
4. **Delete the global guard** from the paths that no longer touch shared state,
   and only then the legacy formal model.

Each step keeps its own KVM gate at 1, then 2, then 4 and 8. Dual-write is
forbidden throughout: the divergence sweep is a comparison of two authorities
that already exist, not a second copy maintained in parallel.

`V5-FORMAL-SCHED-019` closes with a refinement model whose variables are the
owner word, the per-CPU queues, the transfer token, `current`, and the transition
stack, and whose properties are exact-one ownership and a queue-to-owner
refinement. It must kill these mutants: guard removal without owner-word
protection, dual divergence between legacy and per-CPU state, transfer token
reuse, and swapping the source and destination halves of a migration.

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
