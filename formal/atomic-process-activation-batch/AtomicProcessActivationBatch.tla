---------------- MODULE AtomicProcessActivationBatch ----------------
EXTENDS Naturals, FiniteSets, Sequences

(***************************************************************************
Models the all-or-nothing publication of one bounded suspended-child cohort.

Policy selects the cohort in initd. Loaderd binds the requester to the
kernel-stamped IPC sender. Ring0 holds ProcBrokerRegistry before acquiring the
Scheduler lock, validates every exact one-shot capability and suspended task,
then publishes every runnable task and consumes every capability in one
rollback-free critical section. A malformed or foreign cohort changes neither
task nor capability state.

Concrete owners:
  * services/initd/src/main.rs
  * services/loaderd/src/main.rs
  * kernel/compat/src/user/syscall/linux/proc_broker_ops.rs
  * kernel/ps/src/multitask/scheduler.rs
***************************************************************************)

CONSTANTS Targets, BatchFirst, BatchSecond, BatchThird, Requester, Foreign,
          Ordinary

Suspended == "suspended"
Runnable == "runnable"
Retired == "retired"

Live == Requester
Consumed == "consumed"
Revoked == "revoked"
ForeignAuthority == Foreign

Idle == "idle"
Committed == "committed"
Rejected == "rejected"
Exited == "exited"

Batch == <<BatchFirst, BatchSecond, BatchThird>>
BatchSet == {Batch[index] : index \in 1..Len(Batch)}

VARIABLES taskState, authority, shapeValid, phase, queue, ordinaryQueue,
          dispatched, replyResumed

vars ==
    <<taskState, authority, shapeValid, phase, queue, ordinaryQueue,
      dispatched, replyResumed>>

Init ==
    /\ taskState = [task \in Targets |-> Suspended]
    /\ authority = [task \in Targets |-> Live]
    /\ shapeValid = TRUE
    /\ phase = Idle
    /\ queue = <<>>
    \* Model a pre-existing ordinary thread-spawn handoff. It is deliberately
    \* disjoint from the atomic cohort custody queue.
    /\ ordinaryQueue = <<Ordinary>>
    /\ dispatched = <<>>
    /\ replyResumed = FALSE

CorruptAuthority(task) ==
    /\ phase = Idle
    /\ task \in BatchSet
    /\ authority[task] = Live
    /\ authority' = [authority EXCEPT ![task] = ForeignAuthority]
    /\ UNCHANGED
        <<taskState, shapeValid, phase, queue, ordinaryQueue,
          dispatched, replyResumed>>

CorruptShape ==
    /\ phase = Idle
    /\ shapeValid
    /\ shapeValid' = FALSE
    /\ UNCHANGED
        <<taskState, authority, phase, queue, ordinaryQueue,
          dispatched, replyResumed>>

BatchPreflightOK ==
    /\ shapeValid
    /\ Len(Batch) \in 1..8
    /\ \A left, right \in 1..Len(Batch):
        left # right => Batch[left] # Batch[right]
    /\ \A task \in BatchSet:
        /\ taskState[task] = Suspended
        /\ authority[task] = Live

CommitBatch ==
    /\ phase = Idle
    /\ BatchPreflightOK
    /\ taskState' =
        [task \in Targets |->
            IF task \in BatchSet THEN Runnable ELSE taskState[task]]
    /\ authority' =
        [task \in Targets |->
            IF task \in BatchSet THEN Consumed ELSE authority[task]]
    /\ phase' = Committed
    /\ queue' = Batch
    /\ dispatched' = <<>>
    /\ replyResumed' = FALSE
    /\ UNCHANGED <<shapeValid, ordinaryQueue>>

DispatchBatchHead ==
    /\ phase = Committed
    /\ Len(queue) > 0
    /\ queue' = Tail(queue)
    /\ dispatched' = Append(dispatched, Head(queue))
    /\ UNCHANGED
        <<taskState, authority, shapeValid, phase, ordinaryQueue,
          replyResumed>>

ResumeLoaderReply ==
    /\ phase = Committed
    /\ queue = <<>>
    /\ dispatched = Batch
    /\ ~replyResumed
    /\ replyResumed' = TRUE
    /\ UNCHANGED
        <<taskState, authority, shapeValid, phase, queue, ordinaryQueue,
          dispatched>>

RejectBatch ==
    /\ phase = Idle
    /\ ~BatchPreflightOK
    /\ phase' = Rejected
    /\ UNCHANGED
        <<taskState, authority, shapeValid, queue, ordinaryQueue,
          dispatched, replyResumed>>

RequesterExit ==
    /\ phase = Idle
    /\ taskState' =
        [task \in Targets |->
            IF task \in BatchSet THEN Retired ELSE taskState[task]]
    /\ authority' =
        [task \in Targets |->
            IF task \in BatchSet THEN Revoked ELSE authority[task]]
    /\ phase' = Exited
    /\ UNCHANGED
        <<shapeValid, queue, ordinaryQueue, dispatched, replyResumed>>

TerminalStutter ==
    /\ (phase \in {Rejected, Exited} \/ (phase = Committed /\ replyResumed))
    /\ UNCHANGED vars

Next ==
    \/ \E task \in Targets: CorruptAuthority(task)
    \/ CorruptShape
    \/ CommitBatch
    \/ DispatchBatchHead
    \/ ResumeLoaderReply
    \/ RejectBatch
    \/ RequesterExit
    \/ TerminalStutter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ taskState \in [Targets -> {Suspended, Runnable, Retired}]
    /\ authority \in
        [Targets -> {Live, Consumed, Revoked, ForeignAuthority}]
    /\ shapeValid \in BOOLEAN
    /\ phase \in {Idle, Committed, Rejected, Exited}
    /\ queue \in Seq(Targets)
    /\ ordinaryQueue \in Seq({Ordinary})
    /\ dispatched \in Seq(Targets)
    /\ replyResumed \in BOOLEAN

BatchIsBoundedAndUnique ==
    /\ Len(Batch) \in 1..8
    /\ \A left, right \in 1..Len(Batch):
        left # right => Batch[left] # Batch[right]

NoPartialPublication ==
    phase = Committed =>
        /\ \A task \in BatchSet: taskState[task] = Runnable
        /\ \A task \in BatchSet: authority[task] = Consumed
        /\ dispatched \o queue = Batch

NoPublicationBeforeCommit ==
    phase # Committed =>
        /\ \A task \in BatchSet: taskState[task] # Runnable
        /\ \A task \in BatchSet: authority[task] # Consumed
        /\ queue = <<>>
        /\ dispatched = <<>>
        /\ ~replyResumed

RunnableRequiresConsumedAuthority ==
    \A task \in BatchSet:
        taskState[task] = Runnable => authority[task] = Consumed

RejectedBatchPreservesTargets ==
    phase = Rejected =>
        \A task \in BatchSet: taskState[task] = Suspended

ExitedRequesterCannotLeaveActivatableOrphans ==
    phase = Exited =>
        \A task \in BatchSet:
            /\ taskState[task] = Retired
            /\ authority[task] = Revoked

CohortFirstTurnsAreFIFO ==
    phase = Committed => dispatched \o queue = Batch

LoaderReplyWaitsForCohortPrefix ==
    replyResumed =>
        /\ phase = Committed
        /\ dispatched = Batch
        /\ queue = <<>>

OrdinarySpawnBacklogIsDisjointAndPreserved ==
    /\ ordinaryQueue = <<Ordinary>>
    /\ Ordinary \notin BatchSet
    /\ (phase = Committed => dispatched \o queue = Batch)

=============================================================================
