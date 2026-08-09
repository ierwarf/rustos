---------------- MODULE SynchronousIpcHandoffConcurrency ----------------
EXTENDS Naturals, FiniteSets, Sequences

(*******************************************************************************
Two-task/two-CPU companion for the post-Scheduler reply-handoff window.

This deliberately models only the costly interleavings that do not fit the
deeper `synchronous-ipc-handoff` lifecycle refinement:

* each terminal reply wake is one atomic Scheduler transaction, while either
  task may commit first and both opaque tokens may coexist;
* each token independently crosses Begin/Finish publication, including two
  publishers targeting the same FIFO or opposite FIFOs;
* owner generation, CPU, state, and runnable fields can independently churn
  for either task between those two checks;
* a RemoteQueued head is consumed but never dispatched, and its target CPU may
  drain it to Local without changing the captured generation;
* retirement removes that slot from every CPU FIFO.

It intentionally excludes generic handoffs, fairness, and slot reuse.  Those
belong to `synchronous-ipc-handoff/SynchronousIpcHandoff.tla`; retaining them
here would obscure the two independent reply-publication races.

Source refinement anchors:
  kernel/ps/src/multitask/scheduler/sync_handoff.rs
  kernel/ps/src/multitask/scheduler/handoffs.rs
  kernel/ps/src/multitask/scheduler/smp.rs
  kernel/ps/src/multitask/scheduler/runqueue.rs
*******************************************************************************)

CONSTANTS Tasks, Cpus, MaxGeneration, MaxQueue

NoCpu == "no-cpu"
NoTask == "no-task"

TaskBlocked == "task-blocked"
TaskRunnable == "task-runnable"
TaskRetired == "task-retired"

OwnerBlocked == "owner-blocked"
Local == "local"
RemoteQueued == "remote-queued"
OwnerRunning == "owner-running"
OwnerRetired == "owner-retired"

TokenNone == "none"
TokenMinted == "minted"
TokenBegun == "begun"
TokenFinished == "finished"
TokenDropped == "dropped"

TokenPhases == {TokenNone, TokenMinted, TokenBegun, TokenFinished, TokenDropped}

\* Task IDs are monotonic identities in the implementation.  Slot reuse is
\* deliberately outside this model, so the static mapping still makes every
\* publication and selection check the exact task identity rather than only a
\* slot number.
TaskId(task) == task

RecordType == [
    slot       : Tasks,
    taskId     : Tasks,
    generation : 1..MaxGeneration,
    cpu        : Cpus
]

TokenRecordType == [
    slot       : Tasks \cup {NoTask},
    taskId     : Tasks \cup {NoTask},
    generation : 0..MaxGeneration,
    cpu        : Cpus \cup {NoCpu}
]

NoRecord == [
    slot       |-> NoTask,
    taskId     |-> NoTask,
    generation |-> 0,
    cpu        |-> NoCpu
]

VARIABLES taskState, ownerGeneration, ownerCpu, ownerState, ownerRunnable,
          replyDonation, tokenPhase, tokenRecord, tokenUses,
          queue, publicationBegins, postAccepted,
          schedulerSerial, replyCommitSerial,
          dispatched, consumedWithoutDispatch,
          publicationObserverFault, postCheckObserverFault,
          dispatchObserverFault, nonLocalConsumeObserverFault

vars == <<taskState, ownerGeneration, ownerCpu, ownerState, ownerRunnable,
          replyDonation, tokenPhase, tokenRecord, tokenUses,
          queue, publicationBegins, postAccepted,
          schedulerSerial, replyCommitSerial,
          dispatched, consumedWithoutDispatch,
          publicationObserverFault, postCheckObserverFault,
          dispatchObserverFault, nonLocalConsumeObserverFault>>

ReplyRecord(task, cpu) == [
    slot       |-> task,
    taskId     |-> TaskId(task),
    generation |-> ownerGeneration[task],
    cpu        |-> cpu
]

QueueIds(records) == {records[index].taskId : index \in 1..Len(records)}

QueueContainsId(records, id) == id \in QueueIds(records)

\* The FIFO is per target CPU.  A second record for an already present task
\* identity retains the existing position; this model has one opaque reply
\* token per task, so the duplicate path is a structural safety guard rather
\* than a second token-consumption path.
EnqueueDeduplicated(records, record) ==
    IF QueueContainsId(records, record.taskId)
    THEN records
    ELSE Append(records, record)

PurgeSlotEverywhere(slot) ==
    [cpu \in Cpus |->
        SelectSeq(queue[cpu], LAMBDA record : record.slot # slot)]

TokenIsLive(task) == tokenPhase[task] \in {TokenMinted, TokenBegun}

\* Production admission predicate corresponding to
\* ReplyWakeHandoff::owner_still_matches / matches_dispatch_owner.  It is
\* intentionally separate from the observer predicate below.
PublicationAdmission(task) ==
    /\ tokenPhase[task] \in {TokenMinted, TokenBegun}
    /\ tokenRecord[task].slot = task
    /\ tokenRecord[task].taskId = TaskId(task)
    /\ tokenRecord[task].generation = ownerGeneration[task]
    /\ tokenRecord[task].cpu = ownerCpu[task]
    /\ taskState[task] = TaskRunnable
    /\ ownerRunnable[task]
    /\ ownerState[task] \in {Local, RemoteQueued}
    /\ ownerCpu[task] \in Cpus

\* Independently spelled publication observer.  The reversed equalities and
\* explicit captured CPU check make a mutation of PublicationAdmission unable
\* to weaken the invariant that detects the bypass.
ObservedPublicationAdmission(task) ==
    /\ tokenPhase[task] \in {TokenMinted, TokenBegun}
    /\ task = tokenRecord[task].slot
    /\ TaskId(task) = tokenRecord[task].taskId
    /\ ownerGeneration[task] = tokenRecord[task].generation
    /\ ownerCpu[task] = tokenRecord[task].cpu
    /\ taskState[task] = TaskRunnable
    /\ ownerRunnable[task] = TRUE
    /\ ownerState[task] = Local \/ ownerState[task] = RemoteQueued
    /\ tokenRecord[task].cpu \in Cpus

\* Selection is deliberately stricter than publication: a RemoteQueued
\* record may retain ordering but never execution authority.
SelectionAdmission(record, cpu) ==
    /\ record \in RecordType
    /\ record.cpu = cpu
    /\ record.slot \in Tasks
    /\ record.taskId = TaskId(record.slot)
    /\ record.generation = ownerGeneration[record.slot]
    /\ ownerCpu[record.slot] = cpu
    /\ taskState[record.slot] = TaskRunnable
    /\ ownerRunnable[record.slot]
    /\ ownerState[record.slot] = Local

\* Independently spelled selector observer.  It keeps current-CPU equality
\* explicit without relying on the production SelectionAdmission predicate.
ObservedDispatchable(record, currentCpu) ==
    /\ record \in RecordType
    /\ record.cpu = currentCpu
    /\ record.taskId = TaskId(record.slot)
    /\ record.generation = ownerGeneration[record.slot]
    /\ currentCpu = ownerCpu[record.slot]
    /\ taskState[record.slot] = TaskRunnable
    /\ ownerRunnable[record.slot]
    /\ ownerState[record.slot] = Local

\* One terminal reply is a single Scheduler transaction.  TLA actions are
\* atomic; schedulerSerial/replyCommitSerial record that neither task is a
\* privileged representative and that the two commits are serialized.
SchedulerReplyWake(task, cpu, state) ==
    /\ task \in Tasks
    /\ cpu \in Cpus
    /\ state \in {Local, RemoteQueued}
    /\ taskState[task] = TaskBlocked
    /\ replyDonation[task]
    /\ tokenPhase[task] = TokenNone
    /\ LET record == ReplyRecord(task, cpu) IN
       /\ taskState' = [taskState EXCEPT ![task] = TaskRunnable]
       /\ ownerCpu' = [ownerCpu EXCEPT ![task] = cpu]
       /\ ownerState' = [ownerState EXCEPT ![task] = state]
       /\ ownerRunnable' = [ownerRunnable EXCEPT ![task] = TRUE]
       /\ replyDonation' = [replyDonation EXCEPT ![task] = FALSE]
       /\ tokenPhase' = [tokenPhase EXCEPT ![task] = TokenMinted]
       /\ tokenRecord' = [tokenRecord EXCEPT ![task] = record]
       /\ schedulerSerial' = schedulerSerial + 1
       /\ replyCommitSerial' = [replyCommitSerial EXCEPT ![task] = schedulerSerial + 1]
       /\ UNCHANGED <<ownerGeneration, tokenUses, queue, publicationBegins,
                       postAccepted, dispatched, consumedWithoutDispatch,
                       publicationObserverFault, postCheckObserverFault,
                       dispatchObserverFault, nonLocalConsumeObserverFault>>

\* Begin is the pre-enqueue check plus exact captured-CPU FIFO mutation.  Both
\* tasks may enter this action independently, including while the other task
\* is already Begun on the same or the other CPU.
BeginReplyTokenPublication(task) ==
    /\ task \in Tasks
    /\ tokenPhase[task] = TokenMinted
    /\ tokenUses[task] = 0
    /\ PublicationAdmission(task)
    /\ LET targetCpu == tokenRecord[task].cpu
           nextQueue == EnqueueDeduplicated(queue[targetCpu], tokenRecord[task])
       IN
       /\ Len(nextQueue) <= MaxQueue
       /\ tokenPhase' = [tokenPhase EXCEPT ![task] = TokenBegun]
       /\ tokenUses' = [tokenUses EXCEPT ![task] = 1]
       /\ queue' = [queue EXCEPT ![targetCpu] = nextQueue]
       /\ publicationBegins' = [publicationBegins EXCEPT ![task] = 1]
       /\ publicationObserverFault' =
           (publicationObserverFault \/ ~ObservedPublicationAdmission(task))
       /\ UNCHANGED <<taskState, ownerGeneration, ownerCpu, ownerState,
                       ownerRunnable, replyDonation, tokenRecord, postAccepted,
                       schedulerSerial, replyCommitSerial, dispatched,
                       consumedWithoutDispatch, postCheckObserverFault,
                       dispatchObserverFault, nonLocalConsumeObserverFault>>

\* Finish is the post-enqueue owner check.  The opaque token is already used
\* at Begin and cannot be sent to a fallback queue whether this check accepts
\* or rejects the retained record.
FinishReplyTokenPublication(task) ==
    /\ task \in Tasks
    /\ tokenPhase[task] = TokenBegun
    /\ LET accepted == PublicationAdmission(task)
       IN
       /\ tokenPhase' = [tokenPhase EXCEPT ![task] = TokenFinished]
       /\ postAccepted' = [postAccepted EXCEPT ![task] = accepted]
       /\ postCheckObserverFault' =
           (postCheckObserverFault \/
             (accepted /\ ~ObservedPublicationAdmission(task)))
       /\ UNCHANGED <<taskState, ownerGeneration, ownerCpu, ownerState,
                       ownerRunnable, replyDonation, tokenRecord, tokenUses,
                       queue, publicationBegins, schedulerSerial,
                       replyCommitSerial, dispatched, consumedWithoutDispatch,
                       publicationObserverFault, dispatchObserverFault,
                       nonLocalConsumeObserverFault>>

\* If the pre-enqueue check is already stale, consuming the token only loses
\* direct-handoff urgency.  Queue mutation appears in no branch of this action.
DropStaleReplyToken(task) ==
    /\ task \in Tasks
    /\ tokenPhase[task] = TokenMinted
    /\ tokenUses[task] = 0
    /\ ~PublicationAdmission(task)
    /\ tokenPhase' = [tokenPhase EXCEPT ![task] = TokenDropped]
    /\ tokenUses' = [tokenUses EXCEPT ![task] = 1]
    /\ UNCHANGED <<taskState, ownerGeneration, ownerCpu, ownerState,
                    ownerRunnable, replyDonation, tokenRecord, queue,
                    publicationBegins, postAccepted, schedulerSerial,
                    replyCommitSerial, dispatched, consumedWithoutDispatch,
                    publicationObserverFault, postCheckObserverFault,
                    dispatchObserverFault, nonLocalConsumeObserverFault>>

\* These owner-word perturbations are independently quantified over Tasks;
\* there is no OneTask/representative-task reduction.  A true migration or
\* state/cpu rewrite bumps generation, while runnable is separately clearable.
ChangeOwnerGeneration(task) ==
    /\ task \in Tasks
    /\ taskState[task] = TaskRunnable
    /\ ownerGeneration[task] < MaxGeneration
    /\ ownerGeneration' = [ownerGeneration EXCEPT ![task] = @ + 1]
    /\ UNCHANGED <<taskState, ownerCpu, ownerState, ownerRunnable,
                    replyDonation, tokenPhase, tokenRecord, tokenUses, queue,
                    publicationBegins, postAccepted, schedulerSerial,
                    replyCommitSerial, dispatched, consumedWithoutDispatch,
                    publicationObserverFault, postCheckObserverFault,
                    dispatchObserverFault, nonLocalConsumeObserverFault>>

MigrateOwner(task, cpu, state) ==
    /\ task \in Tasks
    /\ cpu \in Cpus
    /\ state \in {Local, RemoteQueued}
    /\ taskState[task] = TaskRunnable
    /\ ownerRunnable[task]
    /\ ownerGeneration[task] < MaxGeneration
    /\ ownerGeneration' = [ownerGeneration EXCEPT ![task] = @ + 1]
    /\ ownerCpu' = [ownerCpu EXCEPT ![task] = cpu]
    /\ ownerState' = [ownerState EXCEPT ![task] = state]
    /\ UNCHANGED <<taskState, ownerRunnable, replyDonation, tokenPhase,
                    tokenRecord, tokenUses, queue, publicationBegins,
                    postAccepted, schedulerSerial, replyCommitSerial,
                    dispatched, consumedWithoutDispatch,
                    publicationObserverFault, postCheckObserverFault,
                    dispatchObserverFault, nonLocalConsumeObserverFault>>

\* The packed owner word carries CPU separately from its generation.  This
\* bounded adversarial perturbation proves that captured-target equality is
\* independently necessary even where a generation mutation is not the reason
\* a token goes stale.  It cannot return a live token to its captured CPU.
MoveOwnerCpuOnly(task, cpu, state) ==
    /\ task \in Tasks
    /\ cpu \in Cpus
    /\ state \in {Local, RemoteQueued}
    /\ taskState[task] = TaskRunnable
    /\ ownerRunnable[task]
    /\ TokenIsLive(task) => cpu # tokenRecord[task].cpu
    /\ ownerCpu' = [ownerCpu EXCEPT ![task] = cpu]
    /\ ownerState' = [ownerState EXCEPT ![task] = state]
    /\ UNCHANGED <<taskState, ownerGeneration, ownerRunnable, replyDonation,
                    tokenPhase, tokenRecord, tokenUses, queue,
                    publicationBegins, postAccepted, schedulerSerial,
                    replyCommitSerial, dispatched, consumedWithoutDispatch,
                    publicationObserverFault, postCheckObserverFault,
                    dispatchObserverFault, nonLocalConsumeObserverFault>>

ClearOwnerRunnable(task) ==
    /\ task \in Tasks
    /\ taskState[task] = TaskRunnable
    /\ ownerRunnable[task]
    /\ ownerState[task] \in {Local, RemoteQueued}
    /\ ownerRunnable' = [ownerRunnable EXCEPT ![task] = FALSE]
    /\ UNCHANGED <<taskState, ownerGeneration, ownerCpu, ownerState,
                    replyDonation, tokenPhase, tokenRecord, tokenUses, queue,
                    publicationBegins, postAccepted, schedulerSerial,
                    replyCommitSerial, dispatched, consumedWithoutDispatch,
                    publicationObserverFault, postCheckObserverFault,
                    dispatchObserverFault, nonLocalConsumeObserverFault>>

\* Target mailbox drain is not migration: it preserves the exact generation
\* and CPU captured in the token while making Local selection possible.
DrainRemoteQueued(task) ==
    /\ task \in Tasks
    /\ taskState[task] = TaskRunnable
    /\ ownerRunnable[task]
    /\ ownerState[task] = RemoteQueued
    /\ ownerCpu[task] \in Cpus
    /\ ownerState' = [ownerState EXCEPT ![task] = Local]
    /\ UNCHANGED <<taskState, ownerGeneration, ownerCpu, ownerRunnable,
                    replyDonation, tokenPhase, tokenRecord, tokenUses, queue,
                    publicationBegins, postAccepted, schedulerSerial,
                    replyCommitSerial, dispatched, consumedWithoutDispatch,
                    publicationObserverFault, postCheckObserverFault,
                    dispatchObserverFault, nonLocalConsumeObserverFault>>

\* Execution custody is not direct-handoff custody.  This is a state-only
\* change, so the selection predicate must reject it even with the same task
\* identity, CPU, generation, and runnable bit.
StartRunningOwner(task) ==
    /\ task \in Tasks
    /\ taskState[task] = TaskRunnable
    /\ ownerRunnable[task]
    /\ ownerState[task] = Local
    /\ ownerState' = [ownerState EXCEPT ![task] = OwnerRunning]
    /\ UNCHANGED <<taskState, ownerGeneration, ownerCpu, ownerRunnable,
                    replyDonation, tokenPhase, tokenRecord, tokenUses, queue,
                    publicationBegins, postAccepted, schedulerSerial,
                    replyCommitSerial, dispatched, consumedWithoutDispatch,
                    publicationObserverFault, postCheckObserverFault,
                    dispatchObserverFault, nonLocalConsumeObserverFault>>

\* Retire is terminal for the modeled slot and compacts every CPU FIFO, not
\* just the FIFO captured by its token.  A begun publication may still finish
\* its post-check afterwards, but cannot restore the purged record.
RetireTask(task) ==
    /\ task \in Tasks
    /\ taskState[task] = TaskRunnable
    /\ ownerGeneration[task] < MaxGeneration
    /\ taskState' = [taskState EXCEPT ![task] = TaskRetired]
    /\ ownerGeneration' = [ownerGeneration EXCEPT ![task] = @ + 1]
    /\ ownerCpu' = [ownerCpu EXCEPT ![task] = NoCpu]
    /\ ownerState' = [ownerState EXCEPT ![task] = OwnerRetired]
    /\ ownerRunnable' = [ownerRunnable EXCEPT ![task] = FALSE]
    /\ queue' = PurgeSlotEverywhere(task)
    /\ UNCHANGED <<replyDonation, tokenPhase, tokenRecord, tokenUses,
                    publicationBegins, postAccepted, schedulerSerial,
                    replyCommitSerial, dispatched, consumedWithoutDispatch,
                    publicationObserverFault, postCheckObserverFault,
                    dispatchObserverFault, nonLocalConsumeObserverFault>>

\* A non-local, stale, withdrawn, or wrong-owner FIFO head is consumed but
\* produces no dispatch.  In particular, RemoteQueued is never executable.
ConsumeNonDispatchableHead(cpu) ==
    /\ cpu \in Cpus
    /\ Len(queue[cpu]) > 0
    /\ LET record == Head(queue[cpu]) IN
       /\ ~SelectionAdmission(record, cpu)
       /\ queue' = [queue EXCEPT ![cpu] = Tail(@)]
       /\ consumedWithoutDispatch' = Append(consumedWithoutDispatch, record)
       /\ nonLocalConsumeObserverFault' =
           (nonLocalConsumeObserverFault \/ ObservedDispatchable(record, cpu))
       /\ UNCHANGED <<taskState, ownerGeneration, ownerCpu, ownerState,
                       ownerRunnable, replyDonation, tokenPhase, tokenRecord,
                       tokenUses, publicationBegins, postAccepted,
                       schedulerSerial, replyCommitSerial, dispatched,
                       publicationObserverFault, postCheckObserverFault,
                       dispatchObserverFault>>

\* A direct dispatch consumes the local head only with exact task identity,
\* generation, current CPU, runnable flag, and Local owner custody.
DispatchLocalHead(cpu) ==
    /\ cpu \in Cpus
    /\ Len(queue[cpu]) > 0
    /\ LET record == Head(queue[cpu]) IN
       /\ SelectionAdmission(record, cpu)
       /\ queue' = [queue EXCEPT ![cpu] = Tail(@)]
       /\ dispatched' = Append(dispatched, record)
       /\ dispatchObserverFault' =
           (dispatchObserverFault \/ ~ObservedDispatchable(record, cpu))
       /\ UNCHANGED <<taskState, ownerGeneration, ownerCpu, ownerState,
                       ownerRunnable, replyDonation, tokenPhase, tokenRecord,
                       tokenUses, publicationBegins, postAccepted,
                       schedulerSerial, replyCommitSerial,
                       consumedWithoutDispatch, publicationObserverFault,
                       postCheckObserverFault, nonLocalConsumeObserverFault>>

\* Explicit terminal stutter keeps ordinary deadlock checking enabled while
\* avoiding an artificial liveness/fairness contract in this safety-only model.
QuiescentStutter ==
    /\ \A task \in Tasks:
          tokenPhase[task] \in {TokenFinished, TokenDropped} \/
          taskState[task] = TaskRetired
    /\ \A cpu \in Cpus: Len(queue[cpu]) = 0
    /\ UNCHANGED vars

Next ==
    \/ \E task \in Tasks, cpu \in Cpus, state \in {Local, RemoteQueued}:
          SchedulerReplyWake(task, cpu, state)
    \/ \E task \in Tasks: BeginReplyTokenPublication(task)
    \/ \E task \in Tasks: FinishReplyTokenPublication(task)
    \/ \E task \in Tasks: DropStaleReplyToken(task)
    \/ \E task \in Tasks: ChangeOwnerGeneration(task)
    \/ \E task \in Tasks, cpu \in Cpus, state \in {Local, RemoteQueued}:
          MigrateOwner(task, cpu, state)
    \/ \E task \in Tasks, cpu \in Cpus, state \in {Local, RemoteQueued}:
          MoveOwnerCpuOnly(task, cpu, state)
    \/ \E task \in Tasks: ClearOwnerRunnable(task)
    \/ \E task \in Tasks: DrainRemoteQueued(task)
    \/ \E task \in Tasks: StartRunningOwner(task)
    \/ \E task \in Tasks: RetireTask(task)
    \/ \E cpu \in Cpus: ConsumeNonDispatchableHead(cpu)
    \/ \E cpu \in Cpus: DispatchLocalHead(cpu)
    \/ QuiescentStutter

Init ==
    /\ Cardinality(Tasks) = 2
    /\ Cardinality(Cpus) = 2
    /\ MaxGeneration >= 2
    /\ MaxQueue = Cardinality(Tasks)
    /\ taskState = [task \in Tasks |-> TaskBlocked]
    /\ ownerGeneration = [task \in Tasks |-> 1]
    /\ ownerCpu = [task \in Tasks |-> NoCpu]
    /\ ownerState = [task \in Tasks |-> OwnerBlocked]
    /\ ownerRunnable = [task \in Tasks |-> FALSE]
    /\ replyDonation = [task \in Tasks |-> TRUE]
    /\ tokenPhase = [task \in Tasks |-> TokenNone]
    /\ tokenRecord = [task \in Tasks |-> NoRecord]
    /\ tokenUses = [task \in Tasks |-> 0]
    /\ queue = [cpu \in Cpus |-> <<>>]
    /\ publicationBegins = [task \in Tasks |-> 0]
    /\ postAccepted = [task \in Tasks |-> FALSE]
    /\ schedulerSerial = 0
    /\ replyCommitSerial = [task \in Tasks |-> 0]
    /\ dispatched = <<>>
    /\ consumedWithoutDispatch = <<>>
    /\ publicationObserverFault = FALSE
    /\ postCheckObserverFault = FALSE
    /\ dispatchObserverFault = FALSE
    /\ nonLocalConsumeObserverFault = FALSE

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ taskState \in [Tasks -> {TaskBlocked, TaskRunnable, TaskRetired}]
    /\ ownerGeneration \in [Tasks -> 1..MaxGeneration]
    /\ ownerCpu \in [Tasks -> (Cpus \cup {NoCpu})]
    /\ ownerState \in [Tasks -> {OwnerBlocked, Local, RemoteQueued, OwnerRunning,
                                  OwnerRetired}]
    /\ ownerRunnable \in [Tasks -> BOOLEAN]
    /\ replyDonation \in [Tasks -> BOOLEAN]
    /\ tokenPhase \in [Tasks -> TokenPhases]
    /\ tokenRecord \in [Tasks -> TokenRecordType]
    /\ tokenUses \in [Tasks -> 0..1]
    /\ queue \in [Cpus -> Seq(RecordType)]
    /\ publicationBegins \in [Tasks -> 0..1]
    /\ postAccepted \in [Tasks -> BOOLEAN]
    /\ schedulerSerial \in 0..Cardinality(Tasks)
    /\ replyCommitSerial \in [Tasks -> 0..Cardinality(Tasks)]
    /\ dispatched \in Seq(RecordType)
    /\ consumedWithoutDispatch \in Seq(RecordType)
    /\ publicationObserverFault \in BOOLEAN
    /\ postCheckObserverFault \in BOOLEAN
    /\ dispatchObserverFault \in BOOLEAN
    /\ nonLocalConsumeObserverFault \in BOOLEAN

OwnerShapeIsSourceFaithful ==
    /\ \A task \in Tasks:
          taskState[task] = TaskBlocked =>
            /\ ownerCpu[task] = NoCpu
            /\ ownerState[task] = OwnerBlocked
            /\ ~ownerRunnable[task]
    /\ \A task \in Tasks:
          taskState[task] = TaskRunnable =>
            /\ ownerCpu[task] \in Cpus
            /\ ownerState[task] \in {Local, RemoteQueued, OwnerRunning}
    /\ \A task \in Tasks:
          taskState[task] = TaskRetired =>
            /\ ownerCpu[task] = NoCpu
            /\ ownerState[task] = OwnerRetired
            /\ ~ownerRunnable[task]

SchedulerReplyWakesAreSerialized ==
    /\ schedulerSerial = Cardinality({task \in Tasks: replyCommitSerial[task] # 0})
    /\ \A left, right \in Tasks:
          /\ left # right
          /\ replyCommitSerial[left] # 0
          /\ replyCommitSerial[right] # 0
          => replyCommitSerial[left] # replyCommitSerial[right]

BothLiveTokensRemainDisjoint ==
    \A left, right \in Tasks:
        /\ left # right
        /\ TokenIsLive(left)
        /\ TokenIsLive(right)
        => /\ tokenRecord[left].slot # tokenRecord[right].slot
           /\ tokenRecord[left].taskId # tokenRecord[right].taskId

TokenUseIsAtMostOnce ==
    \A task \in Tasks: tokenUses[task] \in 0..1

FifoBounded ==
    \A cpu \in Cpus: Len(queue[cpu]) <= MaxQueue

FifoDeduplicatesPerCpu ==
    \A cpu \in Cpus:
        \A left, right \in 1..Len(queue[cpu]):
            left # right => queue[cpu][left].taskId # queue[cpu][right].taskId

QueuedRecordsStayOnCapturedCpu ==
    \A cpu \in Cpus:
        \A index \in 1..Len(queue[cpu]): queue[cpu][index].cpu = cpu

QueuedRecordsOnlyComeFromBegin ==
    \A cpu \in Cpus:
        \A index \in 1..Len(queue[cpu]):
            LET record == queue[cpu][index] IN
            /\ publicationBegins[record.slot] = 1
            /\ tokenRecord[record.slot] = record

RetirementPurgesEveryCpuFifo ==
    \A task \in Tasks:
        taskState[task] = TaskRetired =>
          \A cpu \in Cpus:
            \A index \in 1..Len(queue[cpu]): queue[cpu][index].slot # task

StaleDropHasNoFallbackPublication ==
    \A task \in Tasks:
        tokenPhase[task] = TokenDropped =>
          /\ publicationBegins[task] = 0
          /\ \A cpu \in Cpus:
               \A index \in 1..Len(queue[cpu]):
                   queue[cpu][index].taskId # tokenRecord[task].taskId

PublicationObserverNeverBypassed == ~publicationObserverFault

PostEnqueueObserverNeverBypassed == ~postCheckObserverFault

DispatchObserverNeverBypassed == ~dispatchObserverFault

NonLocalHeadNeverDispatches == ~nonLocalConsumeObserverFault

=============================================================================
