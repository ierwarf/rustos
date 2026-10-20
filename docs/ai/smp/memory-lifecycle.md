# SMP memory, process and failure contract

[Contract index](../smp-contract.md).

## 8. Address spaces and TLB shootdown

Every address space owns:

- a monotonically increasing TLB generation;
- scheduler/process-generation references that keep the unique
  `ProcessAddressSpace` alive;
- an acquire/release per-CPU active-root publication;
- a bounded shootdown token containing address-space identity, generation,
  range/full mode, target mask, and acknowledgement mask;
- deferred physical-frame reclamation tied to the acknowledged generation.

The required mutation/retirement sequence is:

1. validate the entire mapping operation and reserve fallible bookkeeping;
2. mutate PTEs and advance the address-space generation under its mutation
   lock;
3. for retirement, first prove every task/slot/process reference has been
   detached and no CPU can publish that root again;
4. after the page-table edit, snapshot CPUs whose release-published active root
   matches an address-space mutation; a changed activation writes CR3 before
   publishing, while global mappings and retirement still target every
   shootdown-eligible CPU;
5. publish the token, release the mutation/raw lock, and send typed IPIs;
6. invalidate locally and on every target;
7. wait outside raw locks for generation-matched acknowledgements;
8. reclaim page-table or mapped frames only after all required CPUs
   acknowledge.

Range invalidation may be selected only by a measured threshold. Correctness
never depends on that threshold. A missing acknowledgement triggers a 100 ms
diagnostic/resend cadence while the durable transaction and reclaim quarantine
remain owned. Only a 2-second CPU-liveness failure may invoke the boot-only
fail-stop policy; the elapsed 100 ms interval alone is never a panic reason.

The unacknowledged-target policy is **system fail-stop**, chosen explicitly
over CPU quarantine because the supported envelope has no hot-remove: a target
that never acknowledges cannot be retired, so its outstanding translation
cannot be proven dead. An expired deadline therefore panics with the frame
still held behind its mutation guard. No timeout on this path may ever
authorize reuse; freeing a frame a live CPU can still translate is memory
corruption, not a latency symptom. Adopting quarantine instead would first
require `Draining -> Parked -> Offline` CPU states and proof that the
quarantined CPU owns no runnable task and no reclaim-blocking acknowledgement.

The deadline is a dead-owner horizon, not a latency budget. A virtualized
target may be descheduled by its host for a long interval while remaining
healthy, so the bound is deliberately generous and the retry IPI carries
liveness during it. Treating a host pause as a kernel deadlock is a false
positive that this contract rejects.
Stale acknowledgement, generation wrap, token reuse, or reclaim-before-ack is
an internal memory-isolation failure and panics. An address space cannot be destroyed while
any live reference, active-root owner, or shootdown obligation remains. A root
in `Retiring` or `Reclaimed` cannot acquire a new reference or be activated;
future activation after reclaim is an invariant panic, not a stale scheduler
event to tolerate.

## 9. Process, exec, exit, futex, and ABI

Process generation is the cross-CPU lifetime key. `exec` and exit serialize
against thread attachment, address-space retention, signal delivery, wait
queues, timers, IPC donation, and current/ready ownership on every CPU.

- Exit first seals admission, then requests remote task retirement, waits with
  a bounded generation-matched protocol, clears wait/timer/donation authority,
  performs final shootdown, and only then reclaims the address space.
- Group exit freezes exact `(slot, UID)` membership under Class55 after pending
  creation joins drain. Its checked exit nonce/record is independent of exec's
  epoch: pending-exit rollback must not erase an existing census or admit new
  creators. Desired retired bindings acknowledge withdrawal, not physical
  quiescence or consumer completion; missing/recycled bindings never ACK.
  Production group collection no longer scans Scheduler contexts. Actual
  effects still use the directory, including self-exit's drained-join guarantee;
  final joined thread publication cannot move out until that guarantee is replaced.
- Initial user-thread reservation and TEB-failure cancellation use own-CPU
  preemption/IRQ plus exact creator pin/unpublished Ticket, not directory admission.
  Creator pin ends before sleepable TEB work; final commit remains guarded with
  exact Join, MM activation and constructor ACK. Class55 waits enter standalone
  preemption before acquisition and mask protected IRQs afterward.
- Last-live authority comes from Class55's pinned per-process terminal turn,
  not Scheduler context enumeration. Pending creation joins refuse before turn
  nonce spend; roots/joins/mask replacement/new exec census and live member
  detach stay closed through exact Class50 producer publication. Complete
  desired-stop observation authorizes exit sealing separately from final
  committed-stop election. Missing/recycled bindings are not retired negatives.
  Identity51 stop precedes committed membership stop; detached rows preserve
  handle/epoch through a non-Copy publication receipt. Effects abandonment
  retains poisoned occupancy, not implicit reopen. Existing frozen exec census
  can retire an exact sibling without admitting new creators. These are not
  full creation/publication/group-exit/image/IRQ/MM/RSP replacement grants.
- Production terminal withdrawal first borrows an exact Class50 producer loan,
  then authenticates hidden UID through Identity51 -> Resources53 outside the
  terminal leaf. Checked claim nonces preserve original reason/detach custody;
  duplicate claims refuse, untouched rollback restores exact deferred ownership,
  and effect abandonment poisons. Final external cleanup and kernel-revocation
  publication consumes the loan together; both consumer ACKs precede reuse.
  Neither a copied reason nor an empty deferred queue grants reset or teardown.
  Constructor/image-writer/close/reuse prefixes refuse terminal binding. These
  loans do not arbitrate process-last-live, exclude future exec images, or grant
  CPU/IRQ/MM/RSP authority; enclosing genuine directory transactions remain.
- Exec captures `ExecMembership`'s exact process/intent/member census under
  Class55 with outstanding creation joins drained, and retains its checked
  epoch/process pin through image completion or exact cancellation. This phase
  excludes new root/child membership and process-mask publication; removal does
  not discard captured UIDs or reopen creation. Already-retired captured siblings
  still wait for real Running/execution-reference drain, without republishing
  terminal tokens. Recycled UIDs and the external request's live caller cannot
  be retired by this census. Independent input admission replaces directory
  lookup, not terminal or CPU/MM return custody: those genuine guards and the
  sibling/HAL inactive-root/shootdown barrier remain required. Post-stage
  abandonment quarantines, rather than implicitly reopening the process.
- The release stack profile eagerly maps every usable stack page above one
  permanent guard page before the task becomes runnable. Consequently a valid
  stack access does not enter `ProcessStateLock` from page-fault context. No
  lazy-growth exception route is compiled into the enabled product. A future
  lazy profile must first add an immutable task/generation plan, an explicit
  `FaultDeferred` worker owner, and a retry/refinement proof; transient lock
  contention may never be reclassified as retirement.
- Exec reserves an exact frozen scheduler target token, releases the raw owner,
  stages the process/FD/MM bundle under `ProcessStateLock`, releases that lock,
  publishes the matching root/context under the scheduler owner, and finally
  makes the staged generation visible under `ProcessStateLock`. Readers retry
  while the lifecycle is staged/publishing. Exit in that interval latches
  `exit_pending` on the token instead of retiring the target and wins after
  finalization. The old bundle remains retained until visibility commit.
  `ProcessStateLock` and any scheduler/run-queue raw owner must never nest in
  either direction; Windows thread creation follows the same reserve,
  initialize, publish rule.
- Linux per-thread state is protected by its own fixed-slot, generation/TID
  checked raw lock. A raw mutable pointer into scheduler `TaskContext` may not
  escape scheduler serialization. Process-state callers take
  `ProcessState -> LinuxThreadState`; signal/lifecycle callers take
  `Scheduler -> LinuxThreadState`; code holding LinuxThreadState may not enter
  either owner.
- Robust-futex owner death uses an address-space-aware atomic user-u32
  compare-exchange. A BSP read/modify/write sequence is not legal in SMP.
- A non-private futex resolves to a stable shared-backing key when one exists;
  anonymous memory without stable shared backing falls back to the exact
  private mm-generation/root/VA key. Kernel-generated robust-list and
  `clear_child_tid` wakeups try the stable shared key first and then the exact
  private key because the userspace private flag is not retained at exit.
- Private futex keys are `(never-reused mm generation, page-table root,
  virtual word)`. Shared keys are `(backing kind, stable object generation,
  byte offset)` and never use a physical frame number; memfd/shared-region
  aliases therefore match across processes while unmap/remap and allocator
  reuse cannot create ABA. WAIT and CMP_REQUEUE retain the exact process/VMA
  generation, acquire the futex bucket, atomically compare the word, and
  publish or mutate queues in that single critical section. Signal wake that
  leaves a waiter in the table completes with EINTR/restart semantics, not
  success.
- MSI-X installation is a revocable transaction. Function mask/enable writes
  require config-space readback. Device unmask, handler/vector ownership, and
  transport/provider publication either all commit, or rollback disables and
  masks the device before revoking the exact handlers and returning vectors.
  A vector becomes boot-permanent only after the last fallible publication.
- Linux `sched_getaffinity` reports the exact effective target-thread mask,
  bounded by the admitted Online set; `sched_setaffinity` commits the
  Linux-defined intersection with Online. Neither operation may substitute a
  fabricated CPU-zero mask or the whole topology for a pinned thread.
- Windows processor/affinity observations use the same logical topology and
  never expose raw APIC identifiers. `NtQuerySystemInformation` publishes the
  documented processor count while every Microsoft-reserved byte remains
  zero. The supported dense single processor group lets kernelbase derive
  `SYSTEM_INFO.dwActiveProcessorMask` from that count without creating a
  private reserved-field ABI.

The Linux observation boundary is refined by
`formal/cpu-affinity-observation/CpuAffinityObservation.tla`. Kernel-compat
resolves the target TID inside the authenticated caller process, snapshots the
exact effective mask and dense `Online` set, and stamps a version, popcount,
Online bitmap, target-process owner, and task mask into the syscalld request.
Syscalld rejects an empty, foreign-owner, stale-version, count-mismatched,
oversized, or reserved-bearing observation; it never fabricates CPU zero. A
partial published topology or task mask outside Online is an internal SMP
invariant violation and panics before it can become application ABI.

Remote retirement timeout, a task running after its generation was sealed, or
non-atomic robust-futex owner-death cleanup is a kernel invariant failure and
panics. Invalid user pointers, masks, and ABI values return bounded errors.

The executable refinement is
`formal/cross-cpu-task-retirement/CrossCpuTaskRetirement.tla`. An externally
targeted exec uses a target-only no-dispatch quiesce state rather than normal
retirement, because normal retirement would detach the process binding before
the replacement commits. Thread attachment is sealed first; remote siblings
leave their CPUs before detach; replacement requires one remaining thread; and
HAL rejects address-space destruction while any shootdown-eligible CPU still
publishes the old root.

Robust-list exit cleanup is refined by
`formal/robust-futex-owner-death/RobustFutexOwnerDeath.tla`. Kernel-mm admits
only a naturally aligned, present, user-accessible, writable `u32` while the
exact process-state lock retains its mapping. OWNER_DIED uses an acquire load
and bounded AcqRel compare-exchange retry, preserving FUTEX_WAITERS; a foreign
owner or exhausted user-contention budget fails cleanup without fabricating
success. `clear_child_tid` uses a release atomic zero store. Both wakes occur
strictly after publication.

AP preemption is refined by
`formal/per-cpu-clockevent-lifecycle/PerCpuClockeventLifecycle.tla`. CPU 0
retains the admitted PIT clockevent during this stage (its rate-generator
divisor is written only when it changes); every AP requires a local APIC (x2APIC
MSRs when supported, otherwise xAPIC MMIO), invariant-TSC calibration, and
CPUID TSC-deadline support. Its LVT is
programmed masked, the first future deadline is published, and only then is the
vector unmasked and the CPU allowed Online. Interrupt entry rearms a strictly
future deadline before scheduler work, never changes the BSP PIT divisor, and
uses local APIC EOI. Missing prerequisites fail AP admission rather than
silently creating a non-preemptible CPU.
The same calibrated TSC rate does not by itself authorize global SMP
timekeeping. A multi-CPU topology runs on HPET until the bounded cross-CPU warp
rendezvous of section 6 admits raw TSC, and stays on HPET permanently if any
application processor fails or does not complete that rendezvous.

## 10. Locking and memory ordering

The lock hierarchy is explicit and mechanically checked. The initial order is:

```text
topology -> process generation -> address-space mutation
         -> ordered run queues -> wait/timer owner -> leaf device/IPC state
```

Exceptions require a named protocol and a formal/source witness. Lockdep keys
include the real CPU index (`lockdep::cpu_identity::current_cpu_index`),
interrupt state, preemption depth, raw/sleepable class, and observed
dependency graph. Cross-CPU recursion is not permitted.

Every non-relaxed atomic or fence adjacent to SMP code has an `ORDERING:`
comment naming the published data and matching acquire/release edge. `Relaxed`
is allowed only for statistics or when another documented synchronization edge
owns correctness. Unsafe CPU-local or trampoline access has a nearby `SAFETY:`
comment naming lifetime, alignment, aliasing, and CPU ownership.

## 11. Failure policy

Use `Result`/error for untrusted or unsupported input:

- malformed ACPI;
- unsupported topology;
- invalid userspace affinity or pointer;
- resource exhaustion before authority publication;
- a requested release profile whose evidence is absent or stale.

Panic immediately for impossible states after admission:

- duplicate task/CPU/address-space ownership;
- invalid CPU lifecycle transition or stale internal generation;
- AP entering Rust without private critical state;
- unknown kernel IPI or impossible acknowledgement;
- lock order/IRQ/preemption contract violation;
- TLB reclaim before complete acknowledgement;
- continuing execution after failed remote retirement;
- generation or identity wrap that could alias live authority.

Panic messages name the invariant, CPU index, APIC ID when known, generation,
task/process/address-space identity, and outstanding mask. Panics are not used
to parse external data and are not recoverability substitutes.

