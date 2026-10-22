# SMP scheduler and handoff contract

[Contract index](../smp-contract.md).

## 7. Scheduler and task ownership

Each online CPU owns one run queue, one remote-wake mailbox, and at most one
current task. The owner word is the queue/execution authority. The global
`Scheduler` catalog, guard, and consumers are deleted; lifecycle, frame,
funding, and cleanup custody belong to independent owners (see
`ownership/execution.md` in the AI docs). This source boundary does not close
hardware, performance, or full-matrix qualification. Each live task is in
exactly one state:

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
Pick hints may narrow policy order but cannot create execution authority. A
foreign idle-steal scan uses a distinct source-migration predicate: it requires `Local(source_cpu) && runnable`, no current or
transition execution owner, target task-and-process affinity, handoff
admission, and a valid immutable frame before the existing source-owner CAS
publishes the target mailbox. Applying the local target-dispatch predicate to a
foreign source is forbidden because it would reject every valid steal.

A stable affinity snapshot's pending-migration intent is not an in-progress
mask publication. It forbids retaining `Current` on the old CPU, but must not
veto selecting a fresh continuation on an included destination CPU. Zero-mask
or overlapping snapshots remain unknown and abort selection. Selection neither
clears the intent nor grants execution: the exact mask/UID/owner claim and
captured-version architecture/RSP placement ACK remain mandatory. Otherwise a
queued destination would wait for an ACK that only its own dispatch can produce.
The born-resource native witness covers both destination admission and retained
Current refusal; an actual 1/2/4/8-CPU qualification is separate runtime evidence.
This matches the excluded-running-thread migration requirement in Linux
[`sched_setaffinity(2)`](https://man7.org/linux/man-pages/man2/sched_setaffinity.2.html).

Per-CPU load balancing is introduced conservatively:

- enqueue locally when affinity and capacity allow;
- wake on the last CPU when it is online and permitted;
- otherwise preserve the slot-spread target while it is at most one queued
  continuation above the least-loaded permitted queue, and override it only
  for an actual greater-than-one imbalance;
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
different slots require TSS/syscall-stack/segment/FS/GS restoration, while
equal slots retain the already-active state. A different slot reloads CR3 only
when its address-space root differs from the outgoing root; threads in one
address space retain the already-active translations. SIMD restore remains
mandatory on both paths because compiler-generated ring0 code may use vector
registers after the save boundary. Address-space activation separately skips a
CR3 write when the requested root equals the CPU's release-published active root. This
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
- A published raw guard captures the dense `logical_index`, architectural APIC
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
  outgoing owner. The original return now seals its CPU, incoming UID/generation,
  saved frame and both stack ranges while the same physical/MM/UID owners are
  retained. The callback reads actual machine RSP and checks incoming inclusion,
  outgoing exclusion and both original bank roles BEFORE clearing Transition or
  ACKing either bank. Arrival uses only scalar comparisons and atomic bank reads;
  no lock, allocation, output, frame dereference or reconstructed grant is allowed.
  Failed arrival quarantines rather than reopening the turn. Native born-resource
  tests explicitly model RSP, reject old/foreign inputs and retain both banks;
  their fixture-only retry preserves the same parked owners and is not production
  recovery. These tests do not establish that hardware installed RSP.
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
- when that exact receiver is blocked and every affinity, scheduling-context,
  domain-budget, idle-owner, lifecycle, and execution-owner constraint admits
  the caller's CPU, the call transfers receiver custody to that CPU. The caller
  blocks in the same transaction, so this preserves runnable capacity and makes
  the reply eligible for the reverse local handoff without two directed
  reschedule IPIs. Any failed admission retains the receiver's prior CPU and
  the existing exact remote-wake path;
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
- when an unrelated fair competitor is runnable, a synchronous handoff chain
  may consume at most eight scheduler-clock quanta; repeated handoffs within
  one quantum charge once, and after the elapsed-time budget one ordinary
  fairness turn is mandatory without consuming or reordering the FIFO;
- speculative endpoint wake hints without a live reply capability remain
  replaceable and below the absolute overdue gate;
- a same-CPU call or reply may skip the FIFO record. `publish_switching`
  keeps the receiver's pinned frame reader from publication, commits the
  `DirectHandoff` owner and hands that reader to `dispatcher::fast_switch` as a
  `DirectSwitch`, which runs the same prologue, original claim and completion
  as FIFO selection. It is taken only when selection would take this handoff
  first: no older record is queued on the CPU, no activation prefix is pending,
  and an exhausted chain (`sync_handoff::chain::chain_exhausted`) is not owed to
  a ready fair competitor. A reply additionally requires a caller that blocked
  on this CPU within the last millisecond; a longer sleeper takes the ordinary
  wake, which lifts its virtual-time floor. Nothing fails after the owner CAS:
  an unadoptable loan publishes the ordinary FIFO record, and an unused
  `DirectSwitch` materializes the receiver back into fair custody on drop.

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
- the sender-stamped receive half arms the scheduler first, then under one
  endpoint slot guard consumes the exact fast frame or priority-queue head, or
  publishes a bounded receiver waiter. Every non-waiting result cancels the
  arm before user copy; `Waiting` still requires scheduler commit, so a racing
  wake withdraws the arm rather than being lost. No user copy or scheduler
  entry occurs under the endpoint guard;
- an expired receive still tries queued delivery but publishes no new waiter.
  Expiry after waiter publication uses the existing timer arm and exact-endpoint
  withdrawal. A committed block performs the handoff immediately; a message
  already queued requests one syscall-tail handoff without another ring3 trap;
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
- the caller retains ProcBrokerRegistry policy custody, while kernel activation
  captures exact binding Cohort before a qualified facade; it never acquires
  the Scheduler directory or holds Identity51/Resources53/Policy42 across the
  bounded authority callback;
- all capability and scheduler preflight completes before any runnable bit,
  spawn FIFO entry, or capability consumption changes;
- after both preflights, exact one-shot capability consumption precedes task
  effects and the group's one admission Release; callback failure leaves targets
  suspended and panics fail-closed. A concurrent terminal stop may hide identity
  metadata, but cannot erase the captured binding or be reawakened;
- the capability-registry guard is released before milestone or diagnostic
  output, so newly runnable siblings cannot contend on a preempted logging
  owner;
- remote reschedule requests are published only after cohort admission,
  frame readers, execution pins and external authority are released. A target
  can consume a request independently of the producer's deferred IPI flush;
  deferring the physical IPI alone is not a release barrier. Every accepted
  remote target is notified once per CPU, including an already-owned/nonempty
  mailbox whose edge notification was coalesced;
- successful publication completes every surviving member before loader reply;
  where latency priority is admitted, the bounded exact cohort FIFO gives its
  first-turn prefix before the synchronous loader reply chain;
- atomic-cohort custody is a dedicated bounded FIFO and must never alias the
  ordinary single-spawn FIFO: pre-existing or later thread-spawn handoffs
  neither disable nor extend the committed cohort prefix;
- empty-queue polling is only a priority hint. Competing disjoint activations
  arbitrate insertion/count under each Policy42 leaf; saturation/duplicate or
  exhausted priority count degrades to ordinary spawn custody, never ring0 panic
  or execution permission. Per-slot suspension/affinity seal publication shares
  Identity51: a conflicting unowned mask replacement refuses before epoch/masks;
  the syscall returns existing MissingTask rather than racing a cold-owner assert;
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

