# RustOS Commercial SMP Contract

**Status:** normative design and release contract. AP startup, CPU-local
execution ownership, serialized multicore dispatch, IPI/TLB/lifetime/futex,
and AP clockevents are implemented. Per-CPU run queues, targeted load
balancing, compatibility completion, and the full runtime/qualification ladder
remain open; commercial SMP acceptance is closed. A QEMU `-smp` value is
configuration, not SMP evidence.

This document is the focused AI context for x86_64 CPU topology, AP startup,
per-CPU state, inter-processor interrupts, scheduling, TLB invalidation, and
cross-CPU process lifetime. Detailed syscall and service wire rules remain in
`contracts-abi.md`; generic proof infrastructure remains in
`contracts-infra.md`.

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
not application capabilities. A stable `CpuIndex` is a dense logical index;
it is never interchangeable with firmware processor UID or APIC ID.

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
- `Starting`: BSP owns the startup mailbox for the exact `(CpuIndex,
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
3. Construct a dense `CpuIndex -> {firmware UID, APIC ID, generation, state}`
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

## 7. Scheduler and task ownership

Each online CPU owns one run queue, one remote-wake mailbox, and at most one
current task. The owner word is the queue/execution authority; the global
`Scheduler` catalog lock and legacy per-context lifecycle fields remain a
serialized control-plane seam, so this is not yet the final scalability
acceptance point. It must nevertheless satisfy the same exact-one state
partition. Each live task is in exactly one state:

```text
Running(cpu, generation)
Ready(queue_cpu, queue_generation)
Armed(cpu, arm_epoch)
Blocked(wait_owner, block_epoch)
Migrating(source_cpu, target_cpu, migration_epoch)
Retired(task_generation)
```

The transition and its ownership transfer linearize together. A task cannot
be current on two CPUs, appear in two queues, or be both queued and current.
Wake-before-block retains the existing arm/recheck/commit guarantee across
CPUs. Migration uses an exact epoch; a stale dequeue, wake, timer, IPI, or
retirement event cannot revive or duplicate a task.

One acquire snapshot authorizes a local dispatch only when it says
`Local(exact_cpu) && runnable`. `claim_dispatch` applies the same predicate
before touching queue membership or attempting an owner CAS, so a blocked task
that temporarily retains local queue custody cannot consume a dispatch turn.
Legacy `context.ready` and pick hints may narrow policy order but cannot create
execution authority. A foreign idle-steal scan uses a distinct source-
migration predicate: it requires `Local(source_cpu) && runnable`, no current or
transition execution owner, target task-and-process affinity, handoff
admission, and a valid immutable frame before the existing source-owner CAS
publishes the target mailbox. Applying the local target-dispatch predicate to a
foreign source is forbidden because it would reject every valid steal.

Per-CPU load balancing is introduced conservatively:

- enqueue locally when affinity and capacity allow;
- wake on the last CPU when it is online and permitted;
- otherwise select the least loaded permitted queue;
- rebalance only at bounded points, never by a global scan in every tick;
- preserve existing System/User fairness and donation semantics globally;
- affinity masks are validated against the admitted online set.

During the serialized-dispatch transition, the exact last CPU is already
retained as scheduler policy metadata. Ordinary fair picks may prefer a
same-class candidate local to the invoking CPU only while its virtual runtime
is within one minimum-granularity unit of the class-global minimum. A larger
lag forces the global minimum. Strict ready-age recovery, exact activation and
IPC handoffs, affinity, idle ownership, and foreign-running-owner exclusion
precede this tie-break. Slot retirement clears the history; profiler locality
state is separate and can never become scheduling authority.

The serialized ready scanner excludes a slot owned by `Current(cpu)` or
`Transition(cpu)` on any CPU before reading or restoring its saved frame. A
same-CPU current-slot exception is not sufficient: a foreign CPU owns that
stack and register image just as exclusively. Exec keeps its current target
non-ready until the matching process and scheduler generations are published.

Until the per-CPU run-queue transition is complete, the serialized dispatcher
must still avoid replaying task-specific architecture state on a same-task
turn. A scheduling result carries its exact source and destination slots:
different slots require CR3/TSS/syscall-stack/segment/FS/GS restoration, while
equal slots retain the already-active state. SIMD restore remains mandatory on
both paths because compiler-generated ring0 code may use vector registers
after the save boundary. Address-space activation separately skips a CR3 write
when the requested root equals the CPU's release-published active root. This
does not replace shootdown: every page-table mutation still flushes all exact
generation targets, and AP Online admission still performs its mandatory CR3
reload to close the parked translation window.

Idle balancing uses the single-owner transfer protocol. Only an Idle-class CPU
with no eligible local continuation scans the fixed admitted CPU set; it picks
one affinity-valid foreign continuation, removes it under the source queue
owner, publishes it to the target mailbox, and drains that mailbox locally.
It never holds two runqueue locks and never sends itself an IPI. Ordinary busy
ticks perform no unbounded foreign scan. Once per staggered eight-tick window,
a busy source CPU may move at most one affinity-valid queued continuation when
its published runnable count exceeds a permitted target by more than one. The
same source-owner CAS and target mailbox perform the transfer without dual-rq
locking; a directed IPI is sent only for a newly pending remote mailbox. This
closes the permanently-runnable imbalance that wake placement and idle stealing
cannot repair. Repeated affinity rehome replaces an older
mailbox record for the same slot, so stale generations cannot amplify one task
into finite-capacity exhaustion. Waiting for an IPI while holding either queue
or mailbox lock is forbidden. Internal duplicate ownership, lost generation,
invalid queue order, or dispatch on an unready CPU panics.

An empty periodic edge is not itself dispatch authority. After CPU0 has
serviced the shared deadline registry, both BSP RTC and PIT/LAPIC leaves retain
the exact current continuation when it is either the sole User task or the
CPU's published Idle task and there is no local queue/mailbox work, foreign
runnable load, deferred reschedule, or user-return request. Each checked source
is independently release/acquire published; a concurrent enqueue still owns a
directed IPI. This keeps quiescent CPUs off the lifecycle-global catalog lock
without polling away a real wake or idle-steal opportunity.

### 7.1 Dispatch, raw-guard, and CPU-affinity linearization

The executable refinement is
`formal/scheduler-cpu-ownership/SchedulerCpuOwnership.tla`. It is below
scheduling policy: fairness or priority cannot authorize a handoff that
violates execution ownership.

- A raw acquisition first reserves an exact per-CPU pending preemption unit.
  Successful acquisition converts that unit to a held raw class; failed
  acquisition cancels it. Both conversions are local-IRQ atomic, and at every
  stable point `preemption_depth == pending_depth + held_depth`.
- A published raw guard captures the dense `CpuIndex`, architectural APIC
  identity, and nonzero per-CPU preemption depth before protected state is
  exposed.
- The task that acquires a raw guard remains the exact current task on that
  CPU until the final nested guard is released. It cannot block, yield,
  migrate, retire, or be replaced by an IRQ dispatch.
- Timer and reschedule IRQs arriving at nonzero preemption depth may publish
  and acknowledge durable work, but cannot enter the scheduler or consume the
  request. The first same-CPU safe point after depth returns to zero consumes
  it.
- Scheduler selection rejects any non-current task that still has a current
  or stack-transition owner on any CPU. It then reserves the incoming task and
  publishes it as the current slot while retaining the outgoing task stack in
  a CPU-local transition slot. The interrupt stub changes `rsp` to the
  validated incoming frame before its lock-free commit callback releases that
  outgoing owner.
- Every admitted task must occupy exactly one execution-authority class:
  current, stack-transition, or ready. Ready removal and stack-switch commit
  are therefore authority transfers, not independent bookkeeping updates; a
  lost-ready mutation must violate the model even when no duplicate owner is
  present. A CPU with a published scheduler guard cannot simultaneously expose
  a stack-transition owner.
- `scheduler-transition-publication` binds outgoing-slot, active-phase,
  incoming-current, and assembly-commit publication to a Loom interleaving
  kernel, Shuttle PCT schedules, and an x86_64 herd7 baseline/mutant pair. An
  observer that sees either incoming current or the active pre-commit phase
  must see the exact outgoing stack slot; the active bit may normally be clear
  after commit. The evidence covers only this registered two-owner protocol;
  target assembly and runtime handoff remain required gates.
  This two-phase edge prevents another CPU from restoring a frame whose stack
  the first CPU is still using. Scheduler-backed task decoration and snapshots
  are unavailable during that short transition rather than fabricating the
  incoming task as fully installed. The global scheduler scratch field is
  never remote ownership evidence.
- Wake distinguishes `Current(cpu)` from `Transition(cpu)`. A current task has
  consumed its frame, so wake only revokes arm/block and leaves it non-ready
  until its trap publishes a new frame. A transition owner has already
  published that frame, so wake must set it ready while dispatch continues to
  reject the slot until assembly release-clears transition ownership. Treating
  both phases as merely "running" is a lost-wake invariant violation.
- Guard release validates the acquisition CPU, APIC identity, exact acquired
  nesting depth, and pending/held accounting before unlocking protected
  state. Unlock, held-class removal, and preemption-unit release form one
  local-IRQ atomic transition. Cross-CPU release, accounting disagreement,
  underflow, overlapping/premature stack transition, or dispatch with a live
  guard is an immediate invariant panic.
- Logging, panic decoration, and observational probes never acquire the
  scheduler merely to obtain task identity while a raw guard is live. Missing
  decoration is permitted; fabricated or blocking identity is forbidden.
- Retired task metadata is detached one slot per bounded scheduler turn.
  Timer cancellation, IPC endpoint/call revocation, descriptor destruction,
  process-table reclamation, address-space destruction, stack deallocation,
  synchronous logging, and every other allocator or cross-subsystem teardown
  run only from fixed cleanup tokens after the global scheduler raw owner and
  local IRQ exclusion have been released. A slot remains quarantined until
  its kernel side-effect token and userspace retirement acknowledgement are
  both consumed. A scheduler turn may publish a fixed lock-free diagnostic
  record, but cannot make remote CPUs spin behind reclamation or output.
- Only `smp-qualification-ready`, `smp-qualification-start`,
  `smp-qualification-finish`, and `smp-qualification-complete` are
  QualificationCritical and use the fixed bounded retry on the nonblocking
  debug output lock. Scheduler measurements make one attempt; ordinary logs
  remain lossy. This prevents simultaneous CPUs from turning a completed
  lifecycle edge into false negative KVM evidence without permitting an
  unbounded IRQ or raw-lock wait.

This follows the Linux raw-spin owner/preemption rule: spinning locks have
strict owner semantics, disable preemption in the non-RT model, and pin a task
against migration. QNX likewise permits scheduling decisions at kernel/IRQ
entry only within its serialized kernel execution contract. RustOS uses
fine-grained tracked raw locks, but preserves the same non-migration and exact
owner-release safety properties.

### 7.2 Boot dependency barrier

SMP must reduce independent initialization latency without weakening service
authority. The startup registry is a dependency graph, not merely a preferred
list:

- initd may activate `netd`, `devmgrd`, and `inputd` consecutively so their
  initialization overlaps with later loader work;
- activation owns only an exact rootd child lease and is not dependency
  readiness;
- spawned-but-unadmitted packages are excluded from dependency satisfaction;
- before `runtimed` or `storaged` starts, initd must observe each exact
  `(service_id, pid)` endpoint and recheck the live endpoint publication;
- a foreign PID, stale lease, child exit, timeout, or partial barrier fails
  closed and cannot be converted into consumer authority.

This follows the seL4 Microkit separation between unsynchronised protection
domain initialization and capability/channel authority: independent
components may initialize concurrently, while a consumer still requires its
declared communication authority. RustOS retains dynamic restart and exact PID
leases, so its executable refinement is
`formal/post-init-bootstrap-barrier/PostInitBootstrapBarrier.tla`.

### 7.3 Supervisor-committed first-turn handoff

Activation is a scheduler transaction, not a replaceable optimization hint:

- every exact supervisor-committed child activation appends one task slot to
  an allocation-free FIFO bounded by `MAX_TASK`;
- the scheduler-slot bound proves capacity for one deduplicated pending
  handoff per live task; reaching the bound with another distinct live task is
  an internal accounting contradiction and panics;
- later child activations, loader replies, and ordinary IPC donation cannot
  overwrite an older activation;
- retirement removes only the exact stale slot and preserves survivor order;
- the strict-System 2 ms ready-age gate may run before the activation FIFO but
  does not consume it, so boot handoff cannot suspend admitted recovery;
- absent an overdue System continuation, the oldest live activation receives
  its first turn before unrelated latency or IPC hints. User progress remains
  a bounded-burst plus charged-vruntime property, not an unadmitted deadline.

This uses the same explicit queue ownership found in the seL4 scheduler
(runnable peers are FIFO within priority) and Linux dispatch queues (a task is
inserted into a concrete FIFO or priority queue, rather than hidden in a
replaceable side hint). The executable refinement is
`formal/bootstrap-activation-handoff/BootstrapActivationHandoff.tla`.

### 7.4 Terminal reply before scheduling-authority release

A boot-critical server may surrender its base System class only after it has
completed the exact terminal reply for the work that justified that class:

- request handling may record a one-shot demotion intent, but must not perform
  the demotion while the reply capability is live;
- loaderd demotes only after the terminal uiserver spawn reply succeeds;
- vfsd demotes only after the handle-bearing uiserver snapshot reply succeeds;
- a failed, cancelled, or rejected reply retains the server's base class so
  recovery and diagnostics cannot be stranded behind ordinary User work;
- successful self-demotion also caps clone-inherited permanent fair weight at
  the nominal user share without raising a lower weight; temporary synchronous
  service urgency remains reply-scoped rather than becoming a base-weight
  privilege;
- the scheduler's reply-scoped donation remains independent and is still
  revoked by the exact reply/cancellation lifecycle.

This matches the send-receive-reply custody made explicit by QNX: the client
remains reply-blocked until `MsgReply`, and priority inheritance exists to keep
the server executing on behalf of that blocked client. seL4 MCS similarly uses
the reply object to track scheduling-context donation and return it on reply.
The executable refinement is
`formal/scheduler-thread-demotion/SchedulerThreadDemotion.tla`.

### 7.5 Synchronous call/reply handoff custody

Every live synchronous IPC transaction creates scheduler-owned execution
custody for the exact peer needed to advance it:

- call enqueue publishes the exact waiting receiver, or the least-vruntime
  runnable worker of a process-owned endpoint, after reply-capability and
  priority-donation authority exist;
- normal and handle-bearing reply completion releases its reply-scoped
  donation, wakes the exact caller, and captures one opaque `{slot, task ID,
  run-owner generation, target CPU}` token under the same Scheduler
  transaction, only after successful reply-capability consumption;
- after that Scheduler transaction drops, the token may enter only its captured
  CPU's FIFO and only while the exact task ID, owner generation, target CPU,
  runnable flag, and `Local`/`RemoteQueued` owner state still match; a stale,
  migrated, retired, or withdrawn token loses urgency without a catalog/global
  hint fallback or second publication;
- selection repeats the exact reply-record custody predicate. A newer reply
  generation for the same task refreshes one stale FIFO position in place;
  an older reply generation or a generic hint cannot weaken that record, and a
  migration away and back cannot revive the prior generation;
- all call and reply publications share one allocation-free FIFO bounded by
  `MAX_TASK`, so concurrent CPUs cannot overwrite an older transaction;
- duplicate publication consumes no capacity, retirement removes only the
  exact stale slot, and overflow is an accounting contradiction that panics;
- the FIFO head runs before unrelated overdue work because the peer is already
  required by a committed synchronous transaction;
- after eight consecutive synchronous handoffs, one ordinary fairness turn is
  mandatory without consuming or reordering the FIFO; the next dispatch
  resumes its head;
- speculative endpoint wake hints without a live reply capability remain
  replaceable and below the absolute overdue gate.

This follows QNX synchronous message passing, where execution transfers
directly through send/receive/reply and the reply makes the client ready, and
seL4 MCS reply objects, which explicitly track and return the caller's donated
scheduling context. The executable refinement is
`formal/synchronous-ipc-handoff/SynchronousIpcHandoff.tla`.

### 7.6 Fused reply-receive phase and scheduler custody

A byte-only single-endpoint service may use one kernel entry to finish its
current call and wait for the next one, matching the `ReplyRecv` shape exposed
by seL4 while preserving RustOS sender identity and deadline semantics:

- wire version, reserved fields, endpoint process ownership, response bytes,
  next-request capacity, and all copyout ranges are checked while the current
  reply capability remains live;
- reply completion is the commit point: only a successful one-shot consume may
  release donation, wake the exact caller, and publish its synchronous FIFO
  handoff;
- the receive half uses the existing endpoint check-arm-recheck transition. A
  committed block performs the handoff immediately; a message already queued
  requests one syscall-tail handoff without an intermediate ring3 receive trap;
- normal errno means pre-commit and a disjoint native error tag means the reply
  committed before receive failed. Ring3 may make one standalone recovery
  attempt only for the proven pre-commit live cap; it never retries a tagged
  completed cap. Reserved or out-of-range result values are fatal ABI
  violations rather than guessed retry states;
- handle transfer is not implicit. A service requiring attached handles stays
  on the separate handle-aware reply and receive operations;
- a dequeued malformed request is still a live caller obligation and must get a
  terminal error reply rather than being abandoned until timeout.
- a service may fuse the next receive only when the completed reply has no
  immediate cleanup or authority transition behind it. Loaderd therefore
  keeps spawn cleanup and bootstrap demotion on the split path, but fuses its
  byte-only no-post-action requests; zero-byte calls are malformed obligations.

The executable phase refinement is
`formal/ipc-reply-recv-transaction/IpcReplyRecvTransaction.tla`.

### 7.7 Kernel-derived endpoint priority ordering

Synchronous service endpoints deliver calls in two FIFO lanes so a live
System dependency cannot remain behind an unrelated ordinary backlog:

- the compat boundary samples the caller's effective scheduler class before
  taking any IPC object lock; request bytes and ring3 protocol fields can
  never select the lane;
- the endpoint owns separate preallocated System and ordinary FIFO lanes, but
  their combined occupancy is still bounded by
  `MAX_ENDPOINT_PENDING_MESSAGES`;
- FIFO is strict within each lane; a queued System call is selected before an
  ordinary call until two consecutive System deliveries have occurred;
- if both lanes remain nonempty after that two-call burst, exactly one
  ordinary call is selected and resets the burst. A System-only queue may
  continue without inventing an idle turn;
- receive capacity failure does not pop a lane or advance the burst, and
  cancellation removes the exact message from either lane before consuming
  its reply authority;
- queue selection, message validation, and committed pop occur under the same
  endpoint slot guard. A selected head mismatch is an internal ownership
  contradiction and panics rather than silently reordering calls.

This follows the priority-ordered IPC used by seL4 MCS and QNX message
channels, while RustOS adds the bounded ordinary reservation that matches its
two-System/one-User scheduler contract. The executable refinement is
`formal/ipc-priority-queue/IpcPriorityQueue.tla`; its semantic mutant removes
the System-first guard and must be rejected by TLC.

### 7.8 Atomic startup-cohort activation

Post-init boot must not serialize independent suspended siblings by handing
the CPU to the first child before the rest are runnable. Initd owns cohort
selection from the signed dependency graph; loaderd binds the request to the
kernel-stamped sender; ring0 owns only the bounded atomic publication:

- a cohort contains 1..=8 unique nonzero PIDs and has a zeroed unused tail;
- every target must carry the exact requester's unconsumed deferred-start
  capability and a valid suspended scheduler context;
- `ProcBrokerRegistry -> Scheduler` is the only lock order;
- all capability and scheduler preflight completes before any runnable bit,
  spawn FIFO entry, or capability consumption changes;
- after both preflights, exact one-shot capability consumption runs while both
  owners remain held and strictly before scheduler publication; failure during
  this bounded commit leaves every target suspended and panics fail-closed;
- the capability-registry guard is released before milestone or diagnostic
  output, so newly runnable siblings cannot contend on a preempted logging
  owner;
- successful publication queues every sibling in cohort order before the
  loader reply; the scheduler gives exactly that bounded cohort its FIFO
  first-turn prefix before resuming the synchronous loader reply chain;
- atomic-cohort custody is a dedicated bounded FIFO and must never alias the
  ordinary single-spawn FIFO: pre-existing or later thread-spawn handoffs
  neither disable nor extend the committed cohort prefix;
- only one multi-task atomic cohort may own that dedicated FIFO at a time;
  overlap is an internal loader/scheduler protocol contradiction and panics;
  rejection changes none;
- impossible failure after preflight is a kernel invariant violation and
  panics, because partial startup publication has no safe userspace repair.

This closes a 1-vCPU boot amplification in which runtimed could consume a full
fairness window and launch uiserver before storaged received its first turn.
The executable refinement is
`formal/atomic-process-activation-batch/AtomicProcessActivationBatch.tla`,
including the rule that the loader reply cannot resume until the committed
cohort FIFO is drained and that unrelated ordinary spawn backlog cannot consume
or suppress it. Its `AuthorityConsumed` state is a verification-only view of
the lock-held interior: it proves that every one-shot capability is consumed
before runnable publication while forbidding requester exit, dispatch, or reply
between those steps. The existing `bootstrap-activation-handoff` model
continues to prove general FIFO first-turn custody after publication.

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
- Exec uses the same sibling-retirement barrier before replacing mappings or
  ABI state.
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
retains the admitted PIT clockevent during this stage; every AP requires local
xAPIC, invariant-TSC calibration, and CPUID TSC-deadline support. Its LVT is
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
include the real `CpuIndex`, interrupt state, preemption depth, raw/sleepable
class, and observed dependency graph. Cross-CPU recursion is not permitted.

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

## 12. Mechanical enforcement

No single checker is sufficient. The release gate combines:

1. private constructors, newtypes, typestate, bounded arrays, and const layout
   assertions;
2. compile-time rejection of BSP globals in an SMP-enabled build;
3. source-contract headers plus mandatory `SAFETY:` and `ORDERING:` comments;
4. runtime assertions and lockdep for high-risk internal invariants;
5. TLA+ models for CPU online, reschedule IPI, task ownership, shootdown,
   process retirement, robust futex, and release admission;
6. source-anchored Loom plus Shuttle PCT protocol kernels and mutation-sensitive
   x86_64 herd7 litmuses for publication, queues, wake/block, and
   acknowledgement races; a closed Kani/Verus proof index for bounded
   unsafe/state-machine code and selective unbounded acknowledgement/state
   partitions;
7. source-conformance witnesses that bind each formal transition to an exact
   test;
8. fault injection for missing/stale AP, IPI, timer, shootdown, retirement, and
   topology messages;
9. KVM evidence for 1/2/4/8 vCPUs and performance evidence from the same source
   tree and artifacts.

The launcher derives multi-vCPU admission from a fresh PR verification seal
whose schema and artifact set bind the exact current source-tree hash.
Checked-in readiness booleans, stale/mismatched evidence, or passing `-smp`
directly are never release evidence. The launched topology becomes accepted
only after every requested logical CPU emits exact online, idle-entry, first
user-dispatch, first clockevent, and—when more than one CPU is requested—
first reschedule-IPI evidence.

Every mandatory SMP milestone is one allocation-free, self-framed v1 record.
The kernel renders the complete line into fixed storage, covers every semantic
field with a deterministic FNV-1a-64 checksum, acquires the shared output lock,
and performs one debugcon transfer. Healthy-runtime diagnostics use the same
serialized path; only the terminal panic path may bypass it. The host accepts
an SMP event only after validating the complete outer record, canonical inner
field order, checksum, CPU/generation arguments, and strictly increasing output
sequence. A surviving `name=... arg0=...` substring inside a torn or interleaved
line is evidence loss, never a lifecycle proof. SMP success evidence schema v6
names v5 as its predecessor and stores every verified event with its source
line, guest timestamp/tick, milestone and output sequences, CPU, arguments,
qualification-local loss counters, and checksum. A QualificationCritical
frame snapshots only the counters lost by earlier QualificationCritical
frames; a critical drop increments both the global diagnostic and local
qualification counters, so a later critical frame fails closed without foreign
diagnostic contamination. Before QEMU launch it seals the exact formal
verification record, RustOS boot/runtime images, the actual read-only attached
DVM block disk path/bytes/SHA-256, its signed `READ_ONLY` transport header and
Linux `BLKROGET` agreement, the signed
`system/boot/early-system.img#apps/smpqual/smpqual.elf` artifact, DVM
kernel/rootfs, and private qualification contract; success publication
revalidates those immutable bytes and writes a unique, non-overwriting vCPU/run
archive under one explicit matrix cohort. An integer marker count alone is not
publishable evidence. The forced private block provider copies and mode-seals
its snapshot, file-syncs it, and containing-directory-syncs it before aperture
creation/signing. It attaches as `readonly=on` `ide-cd` on existing q35 AHCI
`ide.0`; exact built-in `CONFIG_BLK_DEV_SR=y` owns immutable ATAPI while modular
`sd_mod` owns writable AHCI. Signed and host-verified media geometry is
2048/2048 bytes with 512-byte protocol-sector capacity accounting and a
FLUSH-only feature word, never FUA. A generation-bound read-only media-barrier
completion proves VFS-to-Linux-DVM liveness and does not claim backing-image
write durability. For multi-vCPU
topologies the receive-side `smp-cpu-first-reschedule-ipi` event is required;
the sender-side `smp-resched-route` hint cannot substitute for it.

The boot topology gate and the distribution-performance gate are distinct. A
natural boot's first user dispatch proves that every admitted CPU executed at
least one real Ring3 continuation; it does not prove balanced work, fairness,
or affinity correctness. Distribution qualification therefore additionally
uses an exact per-CPU Ring3 workload and records assigned/observed CPU,
task/process identity, completed work, runtime, and first/last timestamps. A
global dispatch/migration total or the number of boot services cannot replace
those per-CPU observations.

The edit/boot loop has a separate `smp-iteration` evidence profile. It reruns
source conformance and the fixed high-risk SMP model set with a 30-second
per-model bound, seals the exact source tree, and permits only a KVM run whose
host timeout is at most 30 seconds. The launcher rejects this profile for FPS,
recovery, physical-GPU, or commercial acceptance. This profile prevents an
unrelated exhaustive model suite from dominating every scheduler correction
without turning a targeted debugging boot into release evidence.

Current implementation note: CR3 activation and user/global page-table
mutation share the generation-bound `tlb-shootdown-lifecycle`; exec/exit and
address-space destruction pass the cross-CPU retirement barrier; robust futex
cleanup uses atomic user words; and every AP arms a TSC-deadline clockevent
before Online. Linux get/set affinity resolves an exact same-process TID,
round-trips a versioned policy stamp, and commits the effective Online
intersection. Windows basic topology keeps reserved output zero, while
Get/SetProcessAffinityMask, SetThreadAffinityMask, and
GetCurrentProcessorNumber use the same dense single-group scheduler state.
The KVM launcher accepts explicit `--rustos-vcpus 2..=8` only with a fresh
source-bound formal seal and then requires the complete per-CPU runtime event
matrix. Commercial release remains closed until the affinity ABI differential,
formal/source gates, and complete 1/2/4/8 runtime/recovery matrix pass.

## 13. Required validation ladder

For an SMP-affecting change set:

1. `cargo xtask dev-plan`, then every selected immediate check;
2. formal registry/source-contract/system-flow checks;
3. targeted unit tests, TLA+ PR models and mutants, the bounded Loom/Shuttle/
   herd7 concurrency triangle, then the proof-indexed Kani and Verus kernels;
4. `cargo xtask check`, `cargo xtask build`, and `cargo xtask verify-dvm`;
5. bounded fault/recovery probes selected by the contract impact;
6. fresh KVM/QEMU commercial runs at 1, 2, 4, and 8 RustOS vCPUs, each for
   90 seconds, with all normal boot/readiness gates and at least 55 FPS in every
   active UI measurement window;
7. per-topology proof that all requested CPUs reached `Online`, ran idle and
   user work, serviced IPI/timer traffic, and produced no panic, lockdep,
   shootdown, stale-generation, or lost-wake marker;
8. `perf record`/report and internal scheduler/lock/IPI/shootdown counters.

Performance work continues while a repeatable material bottleneck remains.
Stop only when profiles show no single avoidable SMP/boot/runtime/ring0 hotspot
with meaningful user-visible or system-wide impact, regressions are absent
across 1/2/4/8 CPUs, and every accepted optimization has before/after evidence.

## 14. Commercial and high-assurance scheduler comparison

The reference systems do not prescribe one universal lock topology. Linux,
FreeBSD ULE, and Zircon use CPU-local runqueues with bounded balancing. XNU
serializes a selected processor-set domain rather than the whole machine, and
Xen Credit2 permits CPU/core/socket/NUMA/shared domains while capping the CPUs
per runqueue. seL4 SMP deliberately uses a big kernel lock, and Zephyr retains
a correct global-lock compatibility mode, showing that a global owner is not
by itself a correctness defect. It is nevertheless incompatible with the
RustOS scalability gate once measured contention makes independent CPUs wait
on ordinary dispatch.

The userspace-server comparison adds a separate obligation. QNX requires a
multithreaded server to keep a receive-blocked worker and applies server boost
when a high-priority sender arrives while no receiver is blocked. seL4 MCS
instead makes a passive server run on the caller's donated scheduling context.
RustOS retains reply-scoped donation; enqueue on a process-owned endpoint now
selects an eligible worker across all CPU runqueues, publishes the handoff in
that worker's CPU policy, and sends the exact directed reschedule request. A
caller-CPU-only worker scan is forbidden. Userspace startup still owns a larger
bounded reconciliation window around individual 100 ms control mutations, so
one transient single-threaded-server head-of-line delay cannot permanently
remove uiserver. Long-running servers that advertise concurrent low-latency
control remain obligated to provide a bounded worker pool; retries are recovery,
not a substitute for server capacity.

RustOS therefore keeps these distinct acceptance statements:

- exact current-task, runqueue, mailbox, affinity, timer, IPI, and retirement
  ownership is a correctness gate;
- CPU-owned fairness/handoff state, idle stealing, staggered one-task active
  balancing, cross-CPU server boost, and directed reschedule IPIs are the
  current scalable dispatch boundary;
- the remaining global `Scheduler` guard is still a lifecycle/catalog and task
  payload serializer, so `R3-PERF-SCHED-011` is not closed until normal local
  selection and accounting stop acquiring it;
- hard realtime or temporal-isolation claims remain forbidden without explicit
  capacity admission, replenishment, and request-budget custody comparable to
  seL4 MCS or QNX adaptive partitions;
- CPU hotplug, NUMA placement, heterogeneous-core policy, and SMT security
  scheduling are outside the current boot-static eight-CPU product envelope
  and must remain rejected rather than partially advertised.

The next lock split must move mutable task scheduling payload beside its exact
runqueue/task owner. Adding multiple locks around the existing monolithic
`Scheduler` would create unsynchronised aliases and is forbidden. The global
catalog may remain for rare create/exec/exit operations, matching XNU/Xen-style
domain serialization, but a timer, local wake, selection, accounting turn, or
idle steal must not require it in the completed backend.

## 15. Primary design references

These sources inform the contract; RustOS does not claim their verification or
compatibility guarantees.

- UEFI Forum, ACPI 6.6 MADT and system-description-table validation:
  <https://uefi.org/specs/ACPI/6.6/05_ACPI_Software_Programming_Model.html>
- Intel Software Developer Manuals, multiprocessor/APIC/interrupt/memory
  architecture: <https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html>
- Intel xAPIC deprecation and x2APIC direction:
  <https://www.intel.com/content/www/us/en/developer/articles/technical/software-security-guidance/technical-documentation/xapic-deprecation-plan.html>
- AMD64 Architecture Programmer's Manual:
  <https://docs.amd.com/v/u/en-US/40332_4.09_APM_PUB>
- seL4 SMP and MCS scheduler material:
  <https://docs.sel4.systems/releases/sel4/6.0.0.html> and
  <https://docs.sel4.systems/Tutorials/mcs.html>; seL4 runnable-queue FIFO
  semantics within priority:
  <https://docs.sel4.systems/Tutorials/threads.html>
- QNX Neutrino SMP scheduling and cross-CPU rescheduling:
  <https://www.qnx.com/developers/docs/8.0/com.qnx.doc.neutrino.sys_arch/topic/kernel_Scheduling_priority.html>
- QNX synchronous send-receive-reply custody and message-driven priority
  inheritance:
  <https://www.qnx.com/developers/docs/8.0/com.qnx.doc.neutrino.sys_arch/topic/ipc_Sync_messaging.html> and
  <https://qnx.com/developers/docs/7.1/com.qnx.doc.neutrino.sys_arch/topic/ipc_Priority_inheritance_messages.html>;
  QNX no-waiter server boost and receive-blocked thread-pool requirement:
  <https://www.qnx.com/developers/docs/8.0/com.qnx.doc.neutrino.sys_arch/topic/ipc_Server_boost.html> and
  <https://www.qnx.com/developers/docs/8.0/com.qnx.doc.neutrino.resmgr/topic/multithread.html>
- L4Re IPC atomicity and timeout/capability rules:
  <https://l4re.org/doc/l4re_concepts_ipc.html>
- Linux CPU hotplug state machine, x86 TLB guidance, lock types, lockdep, and
  LKMM, scheduler domains, and explicit FIFO/priority dispatch-queue custody:
  <https://docs.kernel.org/6.3/core-api/cpu_hotplug.html>,
  <https://docs.kernel.org/arch/x86/tlb.html>,
  <https://www.kernel.org/doc/html/latest/locking/locktypes.html>,
  <https://docs.kernel.org/5.17/locking/lockdep-design.html>, and
  <https://docs.kernel.org/dev-tools/lkmm/docs/litmus-tests.html>,
  <https://cdn.kernel.org/doc/html/latest/scheduler/sched-domains.html>, and
  <https://docs.kernel.org/scheduler/sched-ext.html>
- FreeBSD ULE current source and per-CPU runqueue/stealing history:
  <https://cgit.freebsd.org/src/log/sys/kern/sched_ule.c>
- Zircon CPU-local fair/deadline runqueues, preemption timers, placement, and
  work stealing:
  <https://fuchsia.dev/fuchsia-src/concepts/kernel/kernel_scheduling>
- Apple XNU processor-set locking, chosen-processor dispatch, and directed AST
  policy:
  <https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/sched_prim.c>
- Xen Credit2 topology-selected runqueue domains and maximum CPUs per runqueue:
  <https://xenbits.xen.org/docs/unstable/misc/xen-command-line.html>
- Zephyr SMP spinlock, CPU-mask, directed-IPI, and cascade contracts:
  <https://docs.zephyrproject.org/latest/kernel/services/smp/smp.html>
- RTEMS scheduler instances, processor assignment, affinity, EDF, and explicit
  scheduled/ready/blocked node states:
  <https://docs.rtems.org/docs/main/c-user/scheduling-concepts/smp-schedulers.html>
- Loom bounded concurrency exploration:
  <https://github.com/tokio-rs/loom>
- Shuttle controlled schedule exploration and PCT scheduler:
  <https://docs.rs/shuttle/0.9.1/shuttle/>
- herdtools7 x86_64 memory-model simulation and source installation:
  <https://diy.inria.fr/tuto/mem/index.html> and
  <https://diy.inria.fr/sources/index.html>
- Kani Rust model checking and current function-contract status:
  <https://model-checking.github.io/kani/> and
  <https://model-checking.github.io/kani/crates/doc/kani/contracts/index.html>
