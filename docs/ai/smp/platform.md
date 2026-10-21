# SMP platform, CPU and interrupt contract

[Contract index](../smp-contract.md).

## 1. Supported commercial envelope

The first supported envelope is deliberately finite:

- architecture: x86_64 under the existing UEFI/Q35/KVM product topology;
- logical CPU counts: exactly 1, 2, 4, or 8;
- topology: one UMA scheduling domain, fixed CPUs discovered at boot;
- interrupt mode: x2APIC when supported, otherwise an explicitly validated
  xAPIC path whose APIC IDs fit the supported destination format;
- no CPU hot-unplug, NUMA placement policy, SMT policy promises, or physical
  multi-socket claim in the first release;
- native Linux ELF and Windows PE64 processes retain the same observable ABI
  at every supported CPU count.

An unsupported or malformed topology is rejected before AP publication.
There is no “boot the BSP and silently ignore unexpected APs” commercial
fallback. The 1-vCPU topology remains a supported member of the same contract,
not a separate implementation.

## 2. Owners and authority

Ring0 owns mechanism only:

| Mechanism | Ring0 owner | Policy boundary |
| --- | --- | --- |
| MADT admission and logical CPU inventory | `kernel-hal` | no user policy |
| AP trampoline and CPU-local substrate | `kernel-hal` / `kernel-lowlevel` | no user policy |
| task-to-CPU ownership and dispatch | `kernel-ps` | weights/classes admitted by existing service policy |
| local APIC, IPI vectors, acknowledgements | `kernel-hal` | callers receive narrow operations, never raw destination authority |
| address-space active mask and TLB epochs | `kernel-mm` | mapping policy remains broker/service-owned |
| process/task generation and retirement | `kernel-ps` | lifecycle policy remains service-owned |
| Linux/Windows affinity ABI | `syscalld` and compat services | kernel exposes validated mechanism |

Raw APIC IDs, CPU-local pointers, run-queue internals, and shootdown tokens are
not application capabilities. The stable per-CPU `logical_index: u8`
(`CpuLifecycleSnapshot`/`CpuLifecycleRegistry` in `kernel/hal/src/arch/smp.rs`)
is a dense logical index; it is never interchangeable with firmware processor
UID or APIC ID.

## 3. CPU lifecycle

The current product envelope admits one boot-static generation per discovered
CPU and exactly one state:

```text
Absent -> Discovered -> Starting -> OnlineParked -> SchedulerReady -> Online
                              \-> Failed
Online -> Quarantined -> Failed
```

- `Discovered`: the complete MADT has passed signature, length, checksum,
  entry-size, uniqueness, enabled-state, and capacity checks.
- `Starting`: BSP owns the startup mailbox for the exact `(logical_index,
  generation, APIC ID)` tuple and has issued the architectural INIT/SIPI
  sequence.
- `OnlineParked`: the AP has installed its private stack, GDT/TSS/IST, IDT,
  GS base, syscall state, SIMD state, local APIC state, and CPU-local pointer,
  but cannot accept user tasks.
- `SchedulerReady`: the per-CPU run queue, idle task, timer, IPI receive path,
  lockdep identity, and address-space tracking are published.
- `Online`: dispatch and cross-CPU requests are permitted.
- `Quarantined` / `Failed`: the CPU owns no runnable task or reclaim-blocking
  acknowledgement. The first commercial envelope treats an online CPU failure
  as a fatal kernel invariant failure unless bounded quarantine has been fully
  proven.

No state may be skipped. Publication is release-ordered; observers use acquire
loads. A stale generation can never acknowledge startup, IPI, TLB, task, or
retirement work.

Scheduler image startup is a one-shot BSP owner, not the runtime global
scheduler guard. Exact OnlineParked generations qualify BSP/AP construction;
per-CPU Publishing admission precedes initial bank effects, complete bank ACKs
precede the single Ready image Release, and AP hardware entry consumes a unique
SchedulerReady-generation producer. Initial Current banks precede Current/active
publication and BSP Current-only SIMD save. Runtime directory entry cannot
manufacture a bootstrap Current. Poison forbids retry/restart of a partial image.
These are mechanism/custody guarantees, not physical RSP/CR3/TSS/funding acceptance;
the runtime global guard is deleted, while commercial runtime and hardware
acceptance remain open.

CPU hot-unplug and in-boot restart are not implemented. `Quarantined -> Failed`
is terminal for that boot, and an attempted transition out of `Failed`, reuse
of generation 1, or a stale-generation transition panics. A future restart
feature must add `Draining -> Parked -> Offline -> Starting` with a strictly
new generation and prove mailbox, APIC, timer, run ownership, and TLB cleanup
before it may weaken this fail-stop contract.

## 4. Firmware and topology admission

MADT parsing is one atomic transaction:

1. Validate the SDT header, total length, checksum, local APIC base, and every
   variable-length entry before publishing any CPU.
2. Accept only understood processor entries needed by the supported envelope.
   Reject zero-length, truncated, duplicate UID, duplicate APIC ID, capacity
   overflow, and contradictory enabled/online-capable data.
3. Construct a dense `logical_index -> {firmware UID, APIC ID, generation, state}`
   table. Never allocate or index by raw APIC ID.
4. Select xAPIC/x2APIC mode once. Every CPU observes the same mode before any
   interrupt can be enabled.
5. Publish the inventory atomically or publish nothing.

Malformed firmware is untrusted input and returns a bounded boot admission
error. It does not itself panic. A mismatch after an admitted topology has
been published is an internal invariant violation and panics.

## 5. AP startup and private CPU state

The AP trampoline may touch only identity-mapped trampoline memory and its
generation-bound startup mailbox. Before entering ordinary Rust, an AP must
have:

- its own aligned boot stack and final kernel stack with guard pages;
- private GDT, TSS, RSP0, and IST stacks;
- an installed IDT and interrupts disabled;
- private syscall/SYSRET scratch and kernel-entry stack;
- private SIMD/FPU ownership state;
- private current-task, preemption, interrupt-depth, and lockdep state;
- initialized local APIC identity and error/spurious vectors;
- a valid CPU-local pointer published with release ordering.

The global BSP TSS, syscall scratch, bootstrap stack, scheduler singleton, and
hard-coded lockdep CPU zero are forbidden once `rustos_vcpus > 1`.
An AP that reaches Rust without all private state panics before enabling
interrupts or scheduling a task.

The BSP publishes one immutable task-migration SIMD profile. Its XCR0 mask,
XSAVE/FXSAVE mode, XSAVEOPT use, AVX/AVX2 admission, enabled-byte count, and
per-component CPUID.0Dh layout fingerprint are release-published together.
Every AP configures exactly that profile and panics before Online if any
required feature, byte count, offset, or format differs. CPUID.0Dh:0.EAX/EDX
is a component bitmap; only EBX after XCR0 installation is the enabled-state
byte count. An AP may never downgrade or rewrite the global profile.

Startup waits are bounded and use a generation-matched acknowledgement. The
BSP must not hold a raw spin lock while waiting. Timeout leaves the topology
unpublished and fails the boot; partial commercial SMP is forbidden.

## 6. Interrupt, timer, and IPI protocol

IPI vectors are statically reserved and typed:

- reschedule;
- TLB shootdown;
- call-function / bounded rendezvous;
- stop/quarantine;
- AP startup acknowledgement.

Every request includes an operation generation, target CPU mask, payload
identity, and acknowledgement mask. Sending to an offline or stale-generation
CPU is rejected. Receiving an unknown vector, impossible self/target mask,
duplicate live token, or stale acknowledgement is a kernel invariant failure
and panics.

The reschedule IPI is the only no-payload, no-rendezvous exception: its durable
owner is one packed per-target atomic word containing a pending bit and a
monotonic request sequence, and the fixed IPI is merely a coalesced
notification. The sender admits only an `Online` target from the
immutable boot CPU generation. A receiver interrupted with preemption disabled
acknowledges the APIC but retains the request for a same-CPU syscall, timer, or
`cond_resched` safe point. CPU hot-remove is prohibited until this exception is
replaced by an explicit generation token.

Each CPU also publishes one cache-line-separated lifecycle generation and
monotonic request/notify/consume sequences. One AcqRel CAS publishes the next
sequence together with pending=1, and one AcqRel CAS claims pending together
with its exact sequence. A publication after that claim necessarily creates a
new 0->1 edge; a publication before it is included in the claimed sequence.
Only a remote 0->1 pending edge records notification; a local safe point,
timer, RTC, syscall tail, or remote IPI records consumption after scheduling
work. One notification may cover several requests. Acceptance
requires same-generation `consume_seq >= request_seq_at_stop` and never
requires a self IPI.

An ordinary reschedule request belongs only to the calling CPU and publishes no
remote authority. A runnable-task or lifecycle mutation that needs another CPU
must name the exact runqueue owner and publish that target's durable request
bit. If the raw scheduler owner currently disables preemption, the exact target
bit transfers notification custody to the unlock path. The raw scheduler guard
releases the physical lock and its preemption unit first, then claims only those
target bits and emits their coalesced remote notifications. A concurrent target
publication remains pending for the next unlock. Fixed self IPIs and topology-
wide ordinary reschedule broadcasts are invariant failures.

Handlers are allocation-free, non-blocking, and never acquire a sleepable
lock. They acknowledge the local APIC exactly once. A sender may wait only
outside raw locks, with a monotonic deadline and direct diagnostic on expiry.

NMI uses a dedicated per-CPU IST stack distinct from double fault and machine
check. Its handler is a fixed emergency marker/return leaf: it does not acquire
the debug lock, scheduler, process state, allocator, or any lock that the
interrupted context may hold. General-protection and page-fault diagnostics
record only scalar vector/error/RIP/RSP values; they never dereference the
untrusted saved user RSP or probe diagnostic memory from exception context.
A fatal-user exception publishes the original terminal/cleanup ownership and
returns without waiting for the independent kernel executor's completion fence.
IF is still clear in that callback; waiting can suppress a TLB ACK needed by the
executor's MM teardown. The original cleanup token seals the first user-fault
vector and the process census's final-member election. The interruptible runtime
cleanup consumer records that process's terminal wait status and performs service
close/policy notification before acknowledging the exact token; the exception
callback does neither. Original Current/Outgoing banks and cleanup rows still
require their separate RSP and both consumer ACKs before reuse. This is not
permission to run external cleanup in the faulting user continuation.

Each online CPU owns its local clockevent. Global time remains a validated
monotonic clocksource; clockevent delivery and timekeeping are separate.
Scheduling quantum is time-based and does not shrink as CPU count increases.
Invariant-TSC capability establishes rate stability, not cross-CPU offset.
A one-CPU topology may therefore publish raw TSC directly, while a
multiprocessor topology starts on the validated 64-bit HPET. TSC frequency
remains available for local TSC-deadline clockevents in both cases.

A multiprocessor topology upgrades to raw TSC only through the bounded
cross-CPU warp rendezvous. While each application processor is parked in
`OnlineParked`, it and the boot processor both publish timestamps into one
monotone shared word for a fixed window and record any observation of a local
timestamp below an already-published one. That observation is the property the
kernel actually depends on — no backwards time — rather than an assumption
about hypervisor or firmware synchronization, and it cannot produce a false
positive: a delay between reading the published value and taking the local
timestamp can only make the local sample larger. This is the admission test
used by Linux (`check_tsc_warp`) and FreeBSD (`comp_smp_tsc`).

Admission is fail-closed and exact. Only a measured warp of zero on every
application processor admits the upgrade; a nonzero warp, an uncalibrated rate,
or a rendezvous that does not complete within its bounded deadline leaves the
HPET in place for the whole boot. Promotion is a one-way upgrade performed by
the boot processor while every other CPU is still parked before
`SchedulerReady`, so no CPU can observe the two time domains out of order, and
the new origin is chosen so the first TSC-derived reading is not earlier than
the last HPET-derived one. The admitted skew is published as evidence rather
than assumed.

This matters beyond timekeeping accuracy. The HPET main counter is an MMIO
read, so under a virtualized product topology every timestamp is an exit
serialized against the other CPUs. Scheduler, timeout, and IPC paths sample
time frequently enough that leaving a multiprocessor topology on that source
makes timestamp cost scale with CPU count.

