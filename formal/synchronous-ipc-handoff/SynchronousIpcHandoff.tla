----------------------- MODULE SynchronousIpcHandoff -----------------------
EXTENDS Naturals, FiniteSets, Sequences

(*******************************************************************************
Models the two custody stages of a synchronous IPC direct handoff.

The reply path is deliberately split at the Scheduler boundary:

  1. `CompleteReplyWake` is one Scheduler transaction.  It releases the
     reply-scoped donation, wakes the exact blocked caller, and creates one
     opaque record containing that caller's slot, monotonic task identity,
     run-owner generation, and target CPU.
  2. `BeginReplyTokenPublication` executes after that Scheduler transaction
     has dropped. It checks the captured generation, CPU, runnable bit, and
     Local/RemoteQueued owner before mutating the target FIFO. A separate
     `FinishReplyTokenPublication` rechecks that custody after the mutation.
     Task identity is revalidated when the FIFO record is selected, so a
     retire/reuse race may retain one stale record but cannot dispatch it.
     Otherwise `DropStaleReplyToken` removes urgency; there is no catalog or
     generic-hint fallback and no second publication.

`kernel/ps/src/multitask/scheduler/sync_handoff.rs` owns the per-CPU FIFO.
`kernel/ps/src/multitask/scheduler.rs` creates the reply token under the
catalog transaction, and `kernel/ps/src/multitask/current.rs` consumes it only
after that transaction has returned.  The FIFO remains bounded, deduplicated,
and fair; a stale record already retained by a target FIFO can only be dropped
at its head and cannot become execution authority.
*******************************************************************************)

CONSTANTS Tasks, Cpus, Kinds, MaxHandoffBurst

Blocked == "blocked"
Runnable == "runnable"
Retired == "retired"

Unowned == "unowned"
Local == "local"
RemoteQueued == "remote-queued"
Running == "running"

Call == "call"
Reply == "reply"
NoCpu == "none"
NoKind == "none"

\* Sentinel slot only; reply lifecycle actions quantify every real task.
OneTask == CHOOSE task \in Tasks : TRUE

RecordType == [
    slot            : Tasks,
    taskId          : Nat,
    ownerGeneration : Nat,
    cpu             : Cpus \cup {NoCpu},
    kind            : Kinds \cup {NoKind}
]

NoRecord == [
    slot            |-> OneTask,
    taskId          |-> 0,
    ownerGeneration |-> 0,
    cpu             |-> NoCpu,
    kind            |-> NoKind
]

VARIABLES taskState, taskId, ownerGeneration, ownerCpu, ownerState,
          ownerRunnable, donation,
          tokenLive, tokenRecord, replyTokenUses, published, dispatched, queue,
          handoffBurst, fairnessTurns, publicationPending,
          invalidReplyRefresh, invalidGenericDowngrade,
          invalidReplyPublication, invalidPostCheck, staleDispatch,
          invalidFairnessPrecedence

vars == <<taskState, taskId, ownerGeneration, ownerCpu, ownerState,
          ownerRunnable, donation,
          tokenLive, tokenRecord, replyTokenUses, published, dispatched, queue,
          handoffBurst, fairnessTurns, publicationPending,
          invalidReplyRefresh, invalidGenericDowngrade,
          invalidReplyPublication, invalidPostCheck, staleDispatch,
          invalidFairnessPrecedence>>

ReplyRecord(slot, cpu) == [
    slot            |-> slot,
    taskId          |-> taskId[slot],
    ownerGeneration |-> ownerGeneration[slot],
    cpu             |-> cpu,
    kind            |-> Reply
]

QueueSet(cpu) == {queue[cpu][index] : index \in 1..Len(queue[cpu])}
QueueTaskIds(cpu) == {queue[cpu][index].taskId : index \in 1..Len(queue[cpu])}

AllQueueSet == UNION {QueueSet(cpu) : cpu \in Cpus}

GenericRecordIsLocallyDispatchable(record, cpu) ==
    /\ record \in RecordType
    /\ record.kind = Call
    /\ record.cpu = cpu
    /\ record.taskId = taskId[record.slot]
    /\ taskState[record.slot] = Runnable
    /\ ownerRunnable[record.slot]
    /\ ownerState[record.slot] = Local
    /\ ownerCpu[record.slot] = cpu

\* Reply records, unlike generic ordering hints, retain the owner snapshot
\* through target-FIFO selection.  Keep this predicate distinct from the
\* observer below so a selector mutation cannot weaken its own assertion.
ReplyRecordMatchesLiveOwner(record) ==
    /\ record \in RecordType
    /\ record.kind = Reply
    /\ record.taskId = taskId[record.slot]
    /\ record.ownerGeneration = ownerGeneration[record.slot]
    /\ record.cpu = ownerCpu[record.slot]
    /\ taskState[record.slot] = Runnable
    /\ ownerRunnable[record.slot]
    /\ ownerState[record.slot] \in {Local, RemoteQueued}
    /\ ownerCpu[record.slot] \in Cpus

\* A reply is allowed to retain post-wake ordering while a remote mailbox owns
\* it, but it is not executable until that CPU drains it into Local custody.
\* Keep this selection predicate separate from publication's broader owner
\* predicate so a Local-check mutation cannot weaken its own observer.
ReplyRecordIsLocallyDispatchable(record, cpu) ==
    /\ record \in RecordType
    /\ record.kind = Reply
    /\ record.cpu = cpu
    /\ record.taskId = taskId[record.slot]
    /\ record.ownerGeneration = ownerGeneration[record.slot]
    /\ ownerCpu[record.slot] = cpu
    /\ taskState[record.slot] = Runnable
    /\ ownerRunnable[record.slot]
    /\ ownerState[record.slot] = Local

RecordIsLocallyDispatchable(record, cpu) ==
    IF record.kind = Reply THEN ReplyRecordIsLocallyDispatchable(record, cpu)
    ELSE GenericRecordIsLocallyDispatchable(record, cpu)

\* Publication can keep a same-generation RemoteQueued reply record only as
\* non-authoritative ordering; selection uses RecordIsLocallyDispatchable.
RecordMatchesLiveOwner(record) ==
    IF record.kind = Reply THEN ReplyRecordMatchesLiveOwner(record)
    ELSE GenericRecordIsLocallyDispatchable(record, record.cpu)

\* Kept independently spelled so a mutation in the selector's predicate is
\* observed rather than accidentally weakening the checker with it.
ReplyRecordHasExactLocalDispatchOwner(record, cpu) ==
    /\ record.kind = Reply
    /\ record.taskId = taskId[record.slot]
    /\ ownerGeneration[record.slot] = record.ownerGeneration
    /\ record.cpu = cpu
    /\ ownerCpu[record.slot] = cpu
    /\ taskState[record.slot] = Runnable
    /\ ownerRunnable[record.slot]
    /\ ownerState[record.slot] = Local

\* Publication before and after the FIFO mutation revalidates only the runqueue
\* custody captured by the token. Task identity is deliberately enforced at
\* selection: retirement/reuse may retain one stale record but cannot dispatch it.
TokenMatchesOwner(slot) ==
    /\ tokenLive[slot]
    /\ tokenRecord[slot].kind = Reply
    /\ tokenRecord[slot].ownerGeneration = ownerGeneration[slot]
    /\ tokenRecord[slot].cpu = ownerCpu[slot]
    /\ ownerRunnable[slot]
    /\ ownerState[slot] \in {Local, RemoteQueued}
    /\ ownerCpu[slot] \in Cpus

\* Independent observer for the publication guard and post-enqueue result.
\* Its reversed equalities prevent a production-predicate mutation from also
\* weakening the invariant that observes the bypass.
ObservedTokenMatchesOwner(slot) ==
    /\ tokenLive[slot]
    /\ tokenRecord[slot].kind = Reply
    /\ ownerGeneration[slot] = tokenRecord[slot].ownerGeneration
    /\ ownerCpu[slot] = tokenRecord[slot].cpu
    /\ ownerRunnable[slot]
    /\ ownerState[slot] \in {Local, RemoteQueued}
    /\ ownerCpu[slot] \in Cpus

NoQueuedTaskId(cpu, id) == id \notin QueueTaskIds(cpu)

\* Independently spelled target-local observer. A mutation that turns FIFO
\* deduplication into a global task-ID gate must not also weaken the expected
\* per-CPU enqueue effect.
ObservedNoQueuedTaskIdOnCpu(cpu, id) ==
    id \notin {queue[cpu][index].taskId : index \in 1..Len(queue[cpu])}

QueuedTaskIsCallOnCpu(cpu, id) ==
    \E index \in 1..Len(queue[cpu]) :
        /\ queue[cpu][index].taskId = id
        /\ queue[cpu][index].kind = Call

QueuedTaskIsStaleOnCpu(cpu, id) ==
    \E index \in 1..Len(queue[cpu]) :
        /\ queue[cpu][index].taskId = id
        /\ ~RecordMatchesLiveOwner(queue[cpu][index])

\* Blocking/reply lifecycle operates on a task, not one local FIFO: an old
\* call or stale record on any CPU permits a fresh reply generation to refresh
\* the new target FIFO without globally rewriting the old record.
QueuedTaskIsCall(id) ==
    \E cpu \in Cpus : QueuedTaskIsCallOnCpu(cpu, id)

QueuedTaskIsStale(id) ==
    \E cpu \in Cpus : QueuedTaskIsStaleOnCpu(cpu, id)

ReplyTokenRefreshesExisting(slot) ==
    \E index \in 1..Len(queue[tokenRecord[slot].cpu]) :
        /\ queue[tokenRecord[slot].cpu][index].taskId = tokenRecord[slot].taskId
        /\ (queue[tokenRecord[slot].cpu][index].kind = Call \/
            (queue[tokenRecord[slot].cpu][index].kind = Reply /\
             tokenRecord[slot].ownerGeneration >=
                 queue[tokenRecord[slot].cpu][index].ownerGeneration))

\* Independently spelled observer: mutating the publication predicate must not
\* weaken the invariant that detects a lost generic-to-reply upgrade.
ObservedReplyTokenMustRefreshExisting(slot) ==
    \E index \in 1..Len(queue[tokenRecord[slot].cpu]) :
        /\ queue[tokenRecord[slot].cpu][index].taskId = tokenRecord[slot].taskId
        /\ (queue[tokenRecord[slot].cpu][index].kind = Call \/
            (queue[tokenRecord[slot].cpu][index].kind = Reply /\
             queue[tokenRecord[slot].cpu][index].ownerGeneration <=
                 tokenRecord[slot].ownerGeneration))

ReplaceSameTaskRecord(records, id, replacement) ==
    [index \in 1..Len(records) |->
        IF records[index].taskId = id THEN replacement ELSE records[index]]

RemoveSlotFromAllQueues(slot) ==
    [cpu \in Cpus |->
        SelectSeq(queue[cpu], LAMBDA record : record.slot # slot)]

Init ==
    /\ Cardinality(Tasks) = 2
    /\ Cardinality(Cpus) = 2
    /\ Call \in Kinds
    /\ Reply \in Kinds
    /\ MaxHandoffBurst > 0
    /\ taskState = [slot \in Tasks |-> Blocked]
    /\ taskId = [slot \in Tasks |-> slot]
    /\ ownerGeneration = [slot \in Tasks |-> 1]
    /\ ownerCpu = [slot \in Tasks |-> NoCpu]
    /\ ownerState = [slot \in Tasks |-> Unowned]
    /\ ownerRunnable = [slot \in Tasks |-> FALSE]
    /\ donation = [slot \in Tasks |-> FALSE]
    /\ tokenLive = [slot \in Tasks |-> FALSE]
    /\ tokenRecord = [slot \in Tasks |-> NoRecord]
    /\ replyTokenUses = [slot \in Tasks |-> 0]
    /\ published = {}
    /\ dispatched = {}
    /\ queue = [cpu \in Cpus |-> <<>>]
    /\ handoffBurst = [cpu \in Cpus |-> 0]
    /\ fairnessTurns = [cpu \in Cpus |-> 0]
    /\ publicationPending = [slot \in Tasks |-> FALSE]
    /\ invalidReplyRefresh = FALSE
    /\ invalidGenericDowngrade = FALSE
    /\ invalidReplyPublication = FALSE
    /\ invalidPostCheck = FALSE
    /\ staleDispatch = FALSE
    /\ invalidFairnessPrecedence = FALSE

\* Generic synchronous-call admission retains its exact receiver directly.
\* A same-task generic hint deduplicates in place without replacing stronger
\* reply custody, matching SyncHandoffState::enqueue.
PublishCallHandoff(slot, cpu) ==
    /\ slot \in Tasks
    /\ cpu \in Cpus
    /\ taskState[slot] = Blocked
    /\ ~tokenLive[slot]
    /\ LET record == [
           slot            |-> slot,
           taskId          |-> taskId[slot],
           ownerGeneration |-> ownerGeneration[slot],
           cpu             |-> cpu,
           kind            |-> Call
       ]
           expectedQueue ==
               IF ObservedNoQueuedTaskIdOnCpu(cpu, taskId[slot])
               THEN Append(queue[cpu], record)
               ELSE queue[cpu]
           nextQueue ==
               IF NoQueuedTaskId(cpu, taskId[slot])
               THEN Append(queue[cpu], record)
               ELSE queue[cpu]
       IN
       /\ taskState' = [taskState EXCEPT ![slot] = Runnable]
       /\ ownerCpu' = [ownerCpu EXCEPT ![slot] = cpu]
       /\ ownerState' = [ownerState EXCEPT ![slot] = Local]
       /\ ownerRunnable' = [ownerRunnable EXCEPT ![slot] = TRUE]
       /\ queue' = [queue EXCEPT ![cpu] = nextQueue]
       /\ invalidGenericDowngrade' =
           (invalidGenericDowngrade \/ nextQueue # expectedQueue)
       /\ published' = published \cup {record}
       /\ UNCHANGED <<taskId, ownerGeneration, donation, tokenLive, tokenRecord,
                       replyTokenUses, dispatched, handoffBurst, fairnessTurns,
                       publicationPending,
                       invalidReplyRefresh, invalidFairnessPrecedence,
                       invalidReplyPublication,
                       invalidPostCheck, staleDispatch>>

\* A live reply capability is the sole source of a terminal reply donation.
ReserveReplyDonation(slot) ==
    /\ slot \in Tasks
    /\ slot = OneTask
    /\ taskState[slot] = Blocked
    /\ ~donation[slot]
    /\ ~tokenLive[slot]
    /\ donation' = [donation EXCEPT ![slot] = TRUE]
    /\ UNCHANGED <<taskState, taskId, ownerGeneration, ownerCpu, ownerState,
                    ownerRunnable, tokenLive, tokenRecord, replyTokenUses, published,
                    dispatched, queue, handoffBurst, fairnessTurns,
                    publicationPending,
                    invalidReplyRefresh, invalidGenericDowngrade,
                    invalidFairnessPrecedence, invalidReplyPublication,
                    invalidPostCheck, staleDispatch>>

\* This action is the one Scheduler transaction for a terminal reply.
CompleteReplyWake(slot, cpu, state) ==
    /\ slot \in Tasks
    /\ slot = OneTask
    /\ cpu \in Cpus
    /\ state \in {Local, RemoteQueued}
    /\ taskState[slot] = Blocked
    /\ donation[slot]
    /\ ~tokenLive[slot]
    /\ LET record == ReplyRecord(slot, cpu) IN
       /\ taskState' = [taskState EXCEPT ![slot] = Runnable]
       /\ ownerCpu' = [ownerCpu EXCEPT ![slot] = cpu]
       /\ ownerState' = [ownerState EXCEPT ![slot] = state]
       /\ ownerRunnable' = [ownerRunnable EXCEPT ![slot] = TRUE]
       /\ donation' = [donation EXCEPT ![slot] = FALSE]
       /\ tokenLive' = [tokenLive EXCEPT ![slot] = TRUE]
       /\ tokenRecord' = [tokenRecord EXCEPT ![slot] = record]
       /\ replyTokenUses' = [replyTokenUses EXCEPT ![slot] = 0]
       /\ UNCHANGED <<taskId, ownerGeneration, published, dispatched, queue,
                       handoffBurst, fairnessTurns,
                       publicationPending,
                       invalidReplyRefresh, invalidGenericDowngrade,
                       invalidFairnessPrecedence, invalidReplyPublication,
                       invalidPostCheck, staleDispatch>>

\* The real publication function mutates one target FIFO between two owner
\* checks. Split those points so migration/retirement can race only through a
\* stale retained record; it cannot create authority or a fallback.
BeginReplyTokenPublication(slot) ==
    /\ slot \in Tasks
    /\ ~publicationPending[slot]
    /\ TokenMatchesOwner(slot)
    /\ LET targetCpu == tokenRecord[slot].cpu
           expectedQueue ==
               IF ObservedNoQueuedTaskIdOnCpu(targetCpu, tokenRecord[slot].taskId)
               THEN Append(queue[targetCpu], tokenRecord[slot])
               ELSE IF ObservedReplyTokenMustRefreshExisting(slot)
                    THEN ReplaceSameTaskRecord(queue[targetCpu], tokenRecord[slot].taskId,
                                               tokenRecord[slot])
                    ELSE queue[targetCpu]
           nextQueue ==
               IF NoQueuedTaskId(targetCpu, tokenRecord[slot].taskId)
               THEN Append(queue[targetCpu], tokenRecord[slot])
               ELSE IF ReplyTokenRefreshesExisting(slot)
                    THEN ReplaceSameTaskRecord(queue[targetCpu], tokenRecord[slot].taskId,
                                               tokenRecord[slot])
                    ELSE queue[targetCpu]
       IN
       /\ queue' = [queue EXCEPT ![targetCpu] = nextQueue]
       /\ invalidReplyRefresh' = (invalidReplyRefresh \/ nextQueue # expectedQueue)
       /\ published' = published \cup {tokenRecord[slot]}
       /\ publicationPending' = [publicationPending EXCEPT ![slot] = TRUE]
       /\ replyTokenUses' = [replyTokenUses EXCEPT ![slot] = @ + 1]
       /\ invalidReplyPublication' =
           (invalidReplyPublication \/ ~ObservedTokenMatchesOwner(slot))
       /\ UNCHANGED <<taskState, taskId, ownerGeneration, ownerCpu, ownerState,
                       ownerRunnable, donation, tokenLive, tokenRecord, dispatched,
                       handoffBurst, fairnessTurns,
                       invalidGenericDowngrade, invalidFairnessPrecedence,
                       invalidPostCheck,
                       staleDispatch>>

FinishReplyTokenPublication(slot) ==
    /\ slot \in Tasks
    /\ publicationPending[slot]
    /\ tokenLive[slot]
    /\ invalidPostCheck' =
        (invalidPostCheck \/
            (TokenMatchesOwner(slot) /\ ~ObservedTokenMatchesOwner(slot)))
    /\ publicationPending' = [publicationPending EXCEPT ![slot] = FALSE]
    /\ tokenLive' = [tokenLive EXCEPT ![slot] = FALSE]
    /\ UNCHANGED <<taskState, taskId, ownerGeneration, ownerCpu, ownerState,
                    ownerRunnable, donation, tokenRecord, replyTokenUses, published,
                    dispatched, queue, handoffBurst, fairnessTurns,
                    invalidReplyRefresh, invalidGenericDowngrade,
                    invalidFairnessPrecedence, invalidReplyPublication,
                    staleDispatch>>

\* A stale token loses only direct-handoff urgency.  It cannot re-enter the
\* Scheduler catalog, manufacture a generic hint, or publish another record.
DropStaleReplyToken(slot) ==
    /\ slot \in Tasks
    /\ ~publicationPending[slot]
    /\ tokenLive[slot]
    /\ ~TokenMatchesOwner(slot)
    /\ tokenLive' = [tokenLive EXCEPT ![slot] = FALSE]
    /\ UNCHANGED <<taskState, taskId, ownerGeneration, ownerCpu, ownerState,
                    ownerRunnable, donation, tokenRecord, replyTokenUses, published,
                    dispatched, queue, handoffBurst, fairnessTurns,
                    publicationPending,
                    invalidReplyRefresh, invalidGenericDowngrade,
                    invalidFairnessPrecedence, invalidReplyPublication,
                    invalidPostCheck, staleDispatch>>

\* Independent owner-field transitions make every token field semantically
\* necessary.  A real migration increments the owner generation, so a return
\* to the captured CPU cannot revive the old record.  The CPU-only action is
\* a one-way field perturbation used to prove that target CPU is independently
\* required; it cannot return to the captured CPU while a token remains live.
\* These adversarial churn actions are symmetric in the task slot, so this
\* deep lifecycle model applies them to OneTask.  A disjoint concurrency model
\* owns two-task Local/RemoteQueued races; keeping that Cartesian product out
\* of this model preserves each individual owner-churn/stale-fault path.
ChangeOwnerGeneration(slot) ==
    /\ slot \in Tasks
    /\ slot = OneTask
    /\ taskState[slot] = Runnable
    /\ ownerState[slot] \in {Local, RemoteQueued}
    /\ ownerGeneration[slot] < 3
    /\ ownerGeneration' = [ownerGeneration EXCEPT ![slot] = @ + 1]
    /\ UNCHANGED <<taskState, taskId, ownerCpu, ownerState, ownerRunnable,
                    donation, tokenLive,
                    tokenRecord, replyTokenUses, published, dispatched, queue,
                    handoffBurst, fairnessTurns, publicationPending,
                    invalidReplyRefresh, invalidGenericDowngrade,
                    invalidFairnessPrecedence,
                    invalidReplyPublication, invalidPostCheck, staleDispatch>>

MigrateOwner(slot, cpu, state) ==
    /\ slot \in Tasks
    /\ slot = OneTask
    /\ cpu \in Cpus
    /\ state \in {Local, RemoteQueued}
    /\ taskState[slot] = Runnable
    /\ ownerGeneration[slot] < 3
    /\ ownerCpu' = [ownerCpu EXCEPT ![slot] = cpu]
    /\ ownerState' = [ownerState EXCEPT ![slot] = state]
    /\ ownerGeneration' = [ownerGeneration EXCEPT ![slot] = @ + 1]
    /\ UNCHANGED <<taskState, taskId, ownerRunnable, donation, tokenLive,
                    tokenRecord, replyTokenUses, published, dispatched, queue,
                    handoffBurst, fairnessTurns, publicationPending,
                    invalidReplyRefresh, invalidGenericDowngrade,
                    invalidFairnessPrecedence,
                    invalidReplyPublication, invalidPostCheck, staleDispatch>>

MoveOwnerCpuOnly(slot, cpu, state) ==
    /\ slot \in Tasks
    /\ slot = OneTask
    /\ cpu \in Cpus
    /\ state \in {Local, RemoteQueued}
    /\ taskState[slot] = Runnable
    /\ tokenLive[slot] => cpu # tokenRecord[slot].cpu
    /\ ownerCpu' = [ownerCpu EXCEPT ![slot] = cpu]
    /\ ownerState' = [ownerState EXCEPT ![slot] = state]
    /\ UNCHANGED <<taskState, taskId, ownerGeneration, ownerRunnable,
                    donation, tokenLive,
                    tokenRecord, replyTokenUses, published, dispatched, queue,
                    handoffBurst, fairnessTurns, publicationPending,
                    invalidReplyRefresh, invalidGenericDowngrade,
                    invalidFairnessPrecedence,
                    invalidReplyPublication, invalidPostCheck, staleDispatch>>

\* A target CPU drains its bounded remote wake mailbox before local candidate
\* selection.  This consumes neither task identity nor owner generation: it
\* only converts already-targeted RemoteQueued custody into the exact Local
\* dispatchability required by runqueue::is_local_dispatchable.
DrainRemoteWake(slot) ==
    /\ slot \in Tasks
    /\ taskState[slot] = Runnable
    /\ ownerState[slot] = RemoteQueued
    /\ ownerCpu[slot] \in Cpus
    /\ ownerRunnable[slot]
    /\ ownerState' = [ownerState EXCEPT ![slot] = Local]
    /\ UNCHANGED <<taskState, taskId, ownerGeneration, ownerCpu, ownerRunnable,
                    donation, tokenLive, tokenRecord, replyTokenUses, published,
                    dispatched, queue, handoffBurst, fairnessTurns,
                    publicationPending,
                    invalidReplyRefresh, invalidGenericDowngrade,
                    invalidFairnessPrecedence, invalidReplyPublication,
                    invalidPostCheck, staleDispatch>>

\* The target may dispatch the just-woken task before the producer reaches
\* the FIFO. Running retains the exact CPU/generation/runnable fields while
\* independently invalidating queue custody.
StartRunningOwner(slot) ==
    /\ slot \in Tasks
    /\ slot = OneTask
    /\ taskState[slot] = Runnable
    /\ ownerState[slot] = Local
    /\ ownerState' = [ownerState EXCEPT ![slot] = Running]
    /\ UNCHANGED <<taskState, taskId, ownerGeneration, ownerCpu, ownerRunnable,
                    donation,
                    tokenLive, tokenRecord, replyTokenUses, published,
                    dispatched, queue, handoffBurst, fairnessTurns,
                    publicationPending,
                    invalidReplyRefresh, invalidGenericDowngrade,
                    invalidFairnessPrecedence, invalidReplyPublication,
                    invalidPostCheck, staleDispatch>>

\* The packed owner word can clear runnable in place without changing Local
\* custody, its CPU, or its generation. Keep it distinct from legacy taskState
\* so the publication and selection runnable guards are independently required.
ClearOwnerRunnable(slot) ==
    /\ slot \in Tasks
    /\ slot = OneTask
    /\ taskState[slot] = Runnable
    /\ ownerRunnable[slot]
    /\ ownerState[slot] \in {Local, RemoteQueued}
    /\ ownerRunnable' = [ownerRunnable EXCEPT ![slot] = FALSE]
    /\ UNCHANGED <<taskState, taskId, ownerGeneration, ownerCpu, ownerState,
                    donation, tokenLive, tokenRecord,
                    replyTokenUses, published,
                    dispatched, queue, handoffBurst, fairnessTurns,
                    publicationPending,
                    invalidReplyRefresh, invalidGenericDowngrade,
                    invalidFairnessPrecedence, invalidReplyPublication,
                    invalidPostCheck, staleDispatch>>

\* After an old queued record has gone stale, the same live task can block
\* again for a new reply.  The new token replaces that stale same-task record
\* in place; it never grows the FIFO or creates two task identities there.
BlockForNextReply(slot) ==
    /\ slot \in Tasks
    /\ slot = OneTask
    /\ taskState[slot] = Runnable
    /\ ~tokenLive[slot]
    /\ ownerGeneration[slot] < 3
    /\ (QueuedTaskIsCall(taskId[slot]) \/ QueuedTaskIsStale(taskId[slot]))
    /\ taskState' = [taskState EXCEPT ![slot] = Blocked]
    /\ ownerCpu' = [ownerCpu EXCEPT ![slot] = NoCpu]
    /\ ownerState' = [ownerState EXCEPT ![slot] = Unowned]
    /\ ownerRunnable' = [ownerRunnable EXCEPT ![slot] = FALSE]
    /\ ownerGeneration' = [ownerGeneration EXCEPT ![slot] = @ + 1]
    /\ donation' = [donation EXCEPT ![slot] = TRUE]
    /\ UNCHANGED <<taskId, tokenLive, tokenRecord, replyTokenUses, published,
                    dispatched, queue, handoffBurst, fairnessTurns,
                    publicationPending,
                    invalidReplyRefresh, invalidGenericDowngrade,
                    invalidFairnessPrecedence, invalidReplyPublication,
                    invalidPostCheck, staleDispatch>>

Retire(slot) ==
    /\ slot \in Tasks
    /\ taskState[slot] # Retired
    /\ taskState' = [taskState EXCEPT ![slot] = Retired]
    /\ ownerCpu' = [ownerCpu EXCEPT ![slot] = NoCpu]
    /\ ownerState' = [ownerState EXCEPT ![slot] = Unowned]
    /\ ownerRunnable' = [ownerRunnable EXCEPT ![slot] = FALSE]
    /\ donation' = [donation EXCEPT ![slot] = FALSE]
    /\ queue' = RemoveSlotFromAllQueues(slot)
    /\ UNCHANGED <<taskId, ownerGeneration, tokenLive, tokenRecord,
                    replyTokenUses, published, dispatched, handoffBurst,
                    fairnessTurns, publicationPending,
                    invalidReplyRefresh, invalidGenericDowngrade,
                    invalidFairnessPrecedence,
                    invalidReplyPublication, invalidPostCheck, staleDispatch>>

\* Slot reuse intentionally permits the old owner generation and CPU to
\* coincide; the monotonic task identity remains the required anti-aliasing
\* field for an outstanding stale token.
ReuseSlot(slot, cpu) ==
    /\ slot \in Tasks
    /\ slot = OneTask
    /\ cpu \in Cpus
    /\ taskState[slot] = Retired
    /\ taskId[slot] < slot + Cardinality(Tasks)
    /\ taskState' = [taskState EXCEPT ![slot] = Runnable]
    /\ taskId' = [taskId EXCEPT ![slot] = @ + Cardinality(Tasks)]
    /\ ownerCpu' = [ownerCpu EXCEPT ![slot] = cpu]
    /\ ownerState' = [ownerState EXCEPT ![slot] = Local]
    /\ ownerRunnable' = [ownerRunnable EXCEPT ![slot] = TRUE]
    /\ UNCHANGED <<ownerGeneration, donation, tokenLive, tokenRecord,
                    replyTokenUses, published, dispatched, queue, handoffBurst,
                    fairnessTurns, publicationPending,
                    invalidReplyRefresh, invalidGenericDowngrade,
                    invalidFairnessPrecedence,
                    invalidReplyPublication, invalidPostCheck, staleDispatch>>

\* A record can become stale after a successful target-FIFO insertion.  It is
\* removed only at the FIFO head, so remaining live records retain FIFO order.
\* This is the model counterpart of SyncHandoffState::take_next_ready: a
\* same-generation RemoteQueued reply is retained as ordering only, then
\* consumed without authority unless the target drains it to Local first.
\* The burst cap is checked before this pop, so a forced ordinary turn retains
\* the head exactly as SyncHandoffState::take_next_ready does.
DropNonDispatchableQueuedHandoff(cpu) ==
    /\ cpu \in Cpus
    /\ Len(queue[cpu]) > 0
    /\ handoffBurst[cpu] < MaxHandoffBurst
    /\ Head(queue[cpu]).cpu = cpu
    /\ ~RecordIsLocallyDispatchable(Head(queue[cpu]), cpu)
    /\ queue' = [queue EXCEPT ![cpu] = Tail(queue[cpu])]
    /\ invalidFairnessPrecedence' =
        (invalidFairnessPrecedence \/ handoffBurst[cpu] >= MaxHandoffBurst)
    /\ UNCHANGED <<taskState, taskId, ownerGeneration, ownerCpu, ownerState,
                    ownerRunnable, donation, tokenLive, tokenRecord,
                    replyTokenUses, published,
                    dispatched, handoffBurst, fairnessTurns,
                    publicationPending,
                    invalidReplyRefresh, invalidGenericDowngrade,
                    invalidReplyPublication,
                    invalidPostCheck, staleDispatch>>

DispatchHandoff(cpu) ==
    /\ cpu \in Cpus
    /\ Len(queue[cpu]) > 0
    /\ handoffBurst[cpu] < MaxHandoffBurst
    /\ Head(queue[cpu]).cpu = cpu
    /\ RecordIsLocallyDispatchable(Head(queue[cpu]), cpu)
    /\ dispatched' = dispatched \cup {Head(queue[cpu])}
    /\ queue' = [queue EXCEPT ![cpu] = Tail(queue[cpu])]
    /\ handoffBurst' = [handoffBurst EXCEPT ![cpu] = @ + 1]
    /\ staleDispatch' =
        (staleDispatch \/
            (Head(queue[cpu]).kind = Reply /\
                ~ReplyRecordHasExactLocalDispatchOwner(Head(queue[cpu]), cpu)))
    /\ UNCHANGED <<taskState, taskId, ownerGeneration, ownerCpu, ownerState,
                    ownerRunnable, donation, tokenLive, tokenRecord,
                    replyTokenUses, published,
                    fairnessTurns, publicationPending,
                    invalidReplyRefresh, invalidGenericDowngrade,
                    invalidFairnessPrecedence,
                    invalidReplyPublication, invalidPostCheck>>

FairnessTurn(cpu) ==
    /\ cpu \in Cpus
    /\ Len(queue[cpu]) > 0
    /\ handoffBurst[cpu] = MaxHandoffBurst
    /\ handoffBurst' = [handoffBurst EXCEPT ![cpu] = 0]
    /\ fairnessTurns' = [fairnessTurns EXCEPT ![cpu] = @ + 1]
    /\ UNCHANGED <<taskState, taskId, ownerGeneration, ownerCpu, ownerState,
                    ownerRunnable, donation, tokenLive, tokenRecord,
                    replyTokenUses, published,
                    dispatched, queue, publicationPending,
                    invalidReplyRefresh, invalidGenericDowngrade,
                    invalidFairnessPrecedence,
                    invalidReplyPublication, invalidPostCheck, staleDispatch>>

TerminalStutter ==
    /\ \A cpu \in Cpus : Len(queue[cpu]) = 0
    /\ \A slot \in Tasks : taskState[slot] = Retired \/ ~tokenLive[slot]
    /\ UNCHANGED vars

Next ==
    \/ \E slot \in Tasks, cpu \in Cpus : PublishCallHandoff(slot, cpu)
    \/ \E slot \in Tasks : ReserveReplyDonation(slot)
    \/ \E slot \in Tasks, cpu \in Cpus, state \in {Local, RemoteQueued} :
        CompleteReplyWake(slot, cpu, state)
    \/ \E slot \in Tasks : BeginReplyTokenPublication(slot)
    \/ \E slot \in Tasks : FinishReplyTokenPublication(slot)
    \/ \E slot \in Tasks : DropStaleReplyToken(slot)
    \/ \E slot \in Tasks : ChangeOwnerGeneration(slot)
    \/ \E slot \in Tasks, cpu \in Cpus, state \in {Local, RemoteQueued} :
        MigrateOwner(slot, cpu, state)
    \/ \E slot \in Tasks, cpu \in Cpus, state \in {Local, RemoteQueued} :
        MoveOwnerCpuOnly(slot, cpu, state)
    \/ \E slot \in Tasks : DrainRemoteWake(slot)
    \/ \E slot \in Tasks : StartRunningOwner(slot)
    \/ \E slot \in Tasks : ClearOwnerRunnable(slot)
    \/ \E slot \in Tasks : BlockForNextReply(slot)
    \/ \E slot \in Tasks : Retire(slot)
    \/ \E slot \in Tasks, cpu \in Cpus : ReuseSlot(slot, cpu)
    \/ \E cpu \in Cpus : DropNonDispatchableQueuedHandoff(cpu)
    \/ \E cpu \in Cpus : DispatchHandoff(cpu)
    \/ \E cpu \in Cpus : FairnessTurn(cpu)
    \/ TerminalStutter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ taskState \in [Tasks -> {Blocked, Runnable, Retired}]
    /\ taskId \in [Tasks -> Nat]
    /\ ownerGeneration \in [Tasks -> Nat]
    /\ ownerCpu \in [Tasks -> (Cpus \cup {NoCpu})]
    /\ ownerState \in [Tasks -> {Unowned, Local, RemoteQueued, Running}]
    /\ ownerRunnable \in [Tasks -> BOOLEAN]
    /\ donation \in [Tasks -> BOOLEAN]
    /\ tokenLive \in [Tasks -> BOOLEAN]
    /\ tokenRecord \in [Tasks -> RecordType]
    /\ replyTokenUses \in [Tasks -> Nat]
    /\ published \subseteq RecordType
    /\ dispatched \subseteq RecordType
    /\ queue \in [Cpus -> Seq(RecordType)]
    /\ handoffBurst \in [Cpus -> 0..MaxHandoffBurst]
    /\ fairnessTurns \in [Cpus -> Nat]
    /\ publicationPending \in [Tasks -> BOOLEAN]
    /\ invalidReplyRefresh \in BOOLEAN
    /\ invalidGenericDowngrade \in BOOLEAN
    /\ invalidReplyPublication \in BOOLEAN
    /\ invalidPostCheck \in BOOLEAN
    /\ staleDispatch \in BOOLEAN
    /\ invalidFairnessPrecedence \in BOOLEAN

QueueBounded ==
    \A cpu \in Cpus : Len(queue[cpu]) <= Cardinality(Tasks)

QueueHasNoDuplicates ==
    \A cpu \in Cpus : Len(queue[cpu]) = Cardinality(QueueTaskIds(cpu))

QueueRecordsWerePublished ==
    \A cpu \in Cpus : QueueSet(cpu) \subseteq published

QueueRecordsBelongToTheirCpu ==
    \A cpu \in Cpus :
        \A index \in 1..Len(queue[cpu]) : queue[cpu][index].cpu = cpu

DispatchRequiresPublication == dispatched \subseteq published

FreshReplyTokenReleasedDonationAndWokeCaller ==
    \A slot \in Tasks :
        TokenMatchesOwner(slot) =>
            /\ donation[slot] = FALSE
            /\ taskState[slot] = Runnable
            /\ tokenRecord[slot].kind = Reply

NoStaleTokenFallbackOrPublication == ~invalidReplyPublication

ReplyRefreshMatchesMonotonicQueueSemantics == ~invalidReplyRefresh

GenericNeverDowngradesReplyCustody == ~invalidGenericDowngrade

PostEnqueueSuccessRequiresExactOwner == ~invalidPostCheck

StaleRecordNeverDispatches == ~staleDispatch

\* A reply token may retain a live RemoteQueued owner snapshot during mailbox
\* delivery, but the per-CPU FIFO must consume it as non-authoritative unless
\* DrainRemoteWake has made that exact owner Local first.
RemoteQueuedReplyNeverDispatches == ~staleDispatch

NonDispatchableHeadRespectsFairnessPrecedence == ~invalidFairnessPrecedence

ReplyTokenPublishesAtMostOnce ==
    \A slot \in Tasks : replyTokenUses[slot] <= 1

QueuedReplyIsNewestPublishedGeneration ==
    \A cpu \in Cpus :
        \A index \in 1..Len(queue[cpu]) :
            queue[cpu][index].kind = Reply =>
                \A record \in published :
                    (record.kind = Reply /\ record.taskId = queue[cpu][index].taskId /\
                        record.cpu = cpu) =>
                        record.ownerGeneration <= queue[cpu][index].ownerGeneration

HandoffBurstIsBounded ==
    \A cpu \in Cpus : handoffBurst[cpu] <= MaxHandoffBurst

=============================================================================
