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
| `V5-SCHED-GLOBAL-001` | **Source deletion complete; hardware/performance evidence separate.** The global Scheduler bodies/type are deleted. Original frame/funding, Current/Transition, lifecycle and external-cleanup owners retain the complete custody boundaries in §2.6b–2.7. Older catalog-acquisition measurements describe intermediate implementations, not the current tree. |
| `V5-FORMAL-SCHED-019` | Guard deletion is complete in source; original-owner joint refinement and hardware execution remain distinct evidence boundaries. Section 2.7 |
| `V5-VFSD-HOL-007` | **structure landed, runtime evidence owed.** The receive owner never blocks, the plan carries its mount generation and the commit refuses a stale one, and custody is a bounded two-worker pool. What is still owed is the measured control-lane residence bound in section 3, and the 2005 ms snapshot itself is still unattributed. Section 3 |
| `V5-WAYLAND-HOL-013`, `V5-GPU-UI-OWNER-014` | no `WaylandProtocolOwner`, `SceneOwner`, `GpuSubmissionOwner`, or `FramePlan`. Section 4 |
| `V5-UI-PIPELINE-011` | `frame_seq` exists only inside `uiserver/main.rs` and `loop_timing.rs`; it never reaches the scheduler, IPC, or DVM relay. Section 5 |
| `V5-TLB-SCALE-009` | **structure closed, runtime evidence owed.** `active_root_targeting_is_sound` asserts CR4.PCIDE is clear per admitted CPU, and the unacknowledged-target policy is documented as system fail-stop with the reclaim alternative rejected in writing. What is owed is the measured ACK latency distribution under a delayed vCPU. Section 8 |

`V5-VFSD-HOL-007` being recorded closed while it times out is the reason section
1 exists. The correction above is the reason it re-derives from source instead
of from identifiers.

## 2. Per-CPU scheduler (`V5-SCHED-GLOBAL-001`, `V5-FORMAL-SCHED-019`)

See [Scheduler policy, measurements and ordering](ownership/scheduler-policy.md#2-per-cpu-scheduler-v5-sched-global-001-v5-formal-sched-019).

### 2.1 What the measurement changed about the target

See [Scheduler policy, measurements and ordering](ownership/scheduler-policy.md#21-what-the-measurement-changed-about-the-target).

### 2.1b Where the lock time actually goes, measured at 8 vCPU

See [Scheduler policy, measurements and ordering](ownership/scheduler-policy.md#21b-where-the-lock-time-actually-goes-measured-at-8-vcpu).

### 2.1c A big lock is not automatically the problem

See [Scheduler policy, measurements and ordering](ownership/scheduler-policy.md#21c-a-big-lock-is-not-automatically-the-problem).

### 2.1d What the handoff chain was actually spending, and what removed it

See [Scheduler policy, measurements and ordering](ownership/scheduler-policy.md#21d-what-the-handoff-chain-was-actually-spending-and-what-removed-it).

### 2.2 Reference designs and what each contributes

See [Scheduler policy, measurements and ordering](ownership/scheduler-policy.md#22-reference-designs-and-what-each-contributes).

### 2.3 Structure

See [Scheduler policy, measurements and ordering](ownership/scheduler-policy.md#23-structure).

### 2.4 Writer rules

See [Scheduler policy, measurements and ordering](ownership/scheduler-policy.md#24-writer-rules).

### 2.5 Linearization points and ordering

See [Scheduler policy, measurements and ordering](ownership/scheduler-policy.md#25-linearization-points-and-ordering).

### 2.6 Placement and balance

See [Scheduler policy, measurements and ordering](ownership/scheduler-policy.md#26-placement-and-balance).

### 2.6a Authoritative fair-policy contract

See [Scheduler policy, measurements and ordering](ownership/scheduler-policy.md#26a-authoritative-fair-policy-contract).

### 2.6b Current authority and remaining deletion boundary

See [Original execution and stack custody](ownership/execution.md#26b-current-authority-and-remaining-deletion-boundary).

#### Current deletion inventory (supersedes earlier migration-stage inventories)

See [Original execution and stack custody](ownership/execution.md#current-deletion-inventory-supersedes-earlier-migration-stage-inventories).

#### Strict original return and bounded terminal/reuse join

See [Original execution and stack custody](ownership/execution.md#strict-original-return-and-bounded-terminalreuse-join).

#### Independent Current terminal entry family

See [Original execution and stack custody](ownership/execution.md#independent-current-terminal-entry-family).

#### Production owners

See [Original execution and stack custody](ownership/execution.md#production-owners).

#### Execution funding and stack allocation custody

See [Original execution and stack custody](ownership/execution.md#execution-funding-and-stack-allocation-custody).

#### Independent identity/lifecycle writer

See [Identity, cleanup and affinity ownership](ownership/identity-cleanup.md#independent-identitylifecycle-writer).

#### Prepublication slot custody and domain descriptors

See [Identity, cleanup and affinity ownership](ownership/identity-cleanup.md#prepublication-slot-custody-and-domain-descriptors).

#### Independent external-cleanup owner

See [Identity, cleanup and affinity ownership](ownership/identity-cleanup.md#independent-external-cleanup-owner).

#### Independent retired detachment through final external ACK

See [Identity, cleanup and affinity ownership](ownership/identity-cleanup.md#independent-retired-detachment-through-final-external-ack).

#### Independent complete Windows process affinity

See [Identity, cleanup and affinity ownership](ownership/identity-cleanup.md#independent-complete-windows-process-affinity).

#### Independent Current entry, diagnostic and wait-key fallback

See [Identity, cleanup and affinity ownership](ownership/identity-cleanup.md#independent-current-entry-diagnostic-and-wait-key-fallback).

#### Independent complete Linux and Windows thread affinity

See [Identity, cleanup and affinity ownership](ownership/identity-cleanup.md#independent-complete-linux-and-windows-thread-affinity).

#### Group withdrawal census and independent thread reservation/cancel

See [Identity, cleanup and affinity ownership](ownership/identity-cleanup.md#group-withdrawal-census-and-independent-thread-reservationcancel).

#### Process terminal membership and final last-live election

See [Terminal, exec and activation ownership](ownership/terminal-activation.md#process-terminal-membership-and-final-last-live-election).

#### Independent deferred terminal stop publication

See [Terminal, exec and activation ownership](ownership/terminal-activation.md#independent-deferred-terminal-stop-publication).

#### Independent wake and final terminal effects

See [Terminal, exec and activation ownership](ownership/terminal-activation.md#independent-wake-and-final-terminal-effects).

#### Producer-qualified terminal custody before withdrawal

See [Terminal, exec and activation ownership](ownership/terminal-activation.md#producer-qualified-terminal-custody-before-withdrawal).

#### Frozen exec membership and independent input admission

See [Terminal, exec and activation ownership](ownership/terminal-activation.md#frozen-exec-membership-and-independent-input-admission).

#### State-free facade / boot and diagnostic custody

See [Terminal, exec and activation ownership](ownership/terminal-activation.md#state-free-facade--boot-and-diagnostic-custody).

#### Independent cold domain admission and captured membership

See [Terminal, exec and activation ownership](ownership/terminal-activation.md#independent-cold-domain-admission-and-captured-membership).

#### User-root producer custody before descriptor visibility

See [Terminal, exec and activation ownership](ownership/terminal-activation.md#user-root-producer-custody-before-descriptor-visibility).

#### Dynamic kernel producer versus bootstrap/AP custody

See [Terminal, exec and activation ownership](ownership/terminal-activation.md#dynamic-kernel-producer-versus-bootstrapap-custody).

#### Initial bank admission versus a constructor-visible prefix

See [Terminal, exec and activation ownership](ownership/terminal-activation.md#initial-bank-admission-versus-a-constructor-visible-prefix).

#### Independent complete exec image publication

See [Terminal, exec and activation ownership](ownership/terminal-activation.md#independent-complete-exec-image-publication).

#### Independent suspended activation and authority commit

See [Terminal, exec and activation ownership](ownership/terminal-activation.md#independent-suspended-activation-and-authority-commit).

#### Kernel creation versus producer-last drained reuse

See [Terminal, exec and activation ownership](ownership/terminal-activation.md#kernel-creation-versus-producer-last-drained-reuse).

#### One-shot BSP/AP image and generation-qualified initial producers

See [Terminal, exec and activation ownership](ownership/terminal-activation.md#one-shot-bspap-image-and-generation-qualified-initial-producers).

#### Running return / Blocked wake CAS arbitration

See [Original claim, interval and return ownership](ownership/claim-return.md#running-return--blocked-wake-cas-arbitration).

#### Original Current wait loan and fused pager cancellation

See [Original claim, interval and return ownership](ownership/claim-return.md#original-current-wait-loan-and-fused-pager-cancellation).

#### Original dispatch return and failed fast IPC peer claim

See [Original claim, interval and return ownership](ownership/claim-return.md#original-dispatch-return-and-failed-fast-ipc-peer-claim).

#### Exact selected frame, funding and queue claim transaction

See [Original claim, interval and return ownership](ownership/claim-return.md#exact-selected-frame-funding-and-queue-claim-transaction).

#### Original claimed architecture and SIMD return

See [Original claim, interval and return ownership](ownership/claim-return.md#original-claimed-architecture-and-simd-return).

#### Original exec census versus physical reclaim admission

See [Original claim, interval and return ownership](ownership/claim-return.md#original-exec-census-versus-physical-reclaim-admission).

#### Original terminal census / process donation withdrawal

See [Original claim, interval and return ownership](ownership/claim-return.md#original-terminal-census--process-donation-withdrawal).

#### Original masked exec retry / Current wake entries

See [Original claim, interval and return ownership](ownership/claim-return.md#original-masked-exec-retry--current-wake-entries).

#### Exact reply object custody versus live execution

See [Original claim, interval and return ownership](ownership/claim-return.md#exact-reply-object-custody-versus-live-execution).

#### Original physical donation participants / reservation funding source

See [Original claim, interval and return ownership](ownership/claim-return.md#original-physical-donation-participants--reservation-funding-source).

#### Original normal reply wake/FIFO lifetime and Current self-demotion

See [Original claim, interval and return ownership](ownership/claim-return.md#original-normal-reply-wakefifo-lifetime-and-current-self-demotion).

#### Original typed pager/service wait publication

See [Original claim, interval and return ownership](ownership/claim-return.md#original-typed-pagerservice-wait-publication).

#### Original claim → executing interval → architecture/return (dispatcher WIP)

See [Original claim, interval and return ownership](ownership/claim-return.md#original-claim--executing-interval--architecturereturn-dispatcher-wip).

#### Original whole fast-IPC producer ownership

See [Original claim, interval and return ownership](ownership/claim-return.md#original-whole-fast-ipc-producer-ownership).

#### Original whole independent IRQ enclosure

See [Original claim, interval and return ownership](ownership/claim-return.md#original-whole-independent-irq-enclosure).

#### Joint original class/funding admission through the actual owner CAS

See [Original claim, interval and return ownership](ownership/claim-return.md#joint-original-classfunding-admission-through-the-actual-owner-cas).

#### Obsolete process-donation authority deleted

See [Publication ownership and guard deletion requirements](ownership/publication-deletion.md#obsolete-process-donation-authority-deleted).

#### Original cold IPC producer before sender wait arm

See [Publication ownership and guard deletion requirements](ownership/publication-deletion.md#original-cold-ipc-producer-before-sender-wait-arm).

#### Independent BSP/AP initial execution family

See [Publication ownership and guard deletion requirements](ownership/publication-deletion.md#independent-bspap-initial-execution-family).

#### Independent signal and complete process job-control family

See [Publication ownership and guard deletion requirements](ownership/publication-deletion.md#independent-signal-and-complete-process-job-control-family).

#### Original cold Current observations and final cache effects

See [Publication ownership and guard deletion requirements](ownership/publication-deletion.md#original-cold-current-observations-and-final-cache-effects).

#### Original wake, ordering and activation publication

See [Publication ownership and guard deletion requirements](ownership/publication-deletion.md#original-wake-ordering-and-activation-publication).

### 2.7 Guard deletion requirements

See [Publication ownership and guard deletion requirements](ownership/publication-deletion.md#27-guard-deletion-requirements).

## 3. vfsd receive owner and bulk lane (`V5-VFSD-HOL-007`)

QNX's rule is the whole design: a server should always have a RECEIVE-blocked
thread, and a kernel boost that compensates for the absence of one is working
around a badly built server. The historical single serve loop performed
snapshot reads itself. Current source resolves then enqueues executable
snapshot work into the bounded worker pool instead; control-lane runtime
residence evidence is still owed.

The design separates four owner roles (not necessarily four named types):

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

This ordering records the migration plan, not a current closure inventory.
Section 2's source deletion is complete; hardware/performance qualification
remains separate. Sections 3 and 8 still owe their runtime evidence, and
sections 4 and 5 retain the owner/trace work listed above. Source structure
alone cannot close those gates: the earlier per-slot owner-word failure passed
every unit test and only broke in KVM.

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
torn class publication cannot spend that queue's budget. The former global-guard
first-turn producer, donation serialization, and teardown path were intermediate
migration premises, now superseded by the independent-owner deletion inventory
in §2.6b–2.7. Their source deletion does not establish hardware acceptance.
