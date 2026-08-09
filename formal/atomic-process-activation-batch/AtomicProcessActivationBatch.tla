---------------- MODULE AtomicProcessActivationBatch ----------------
EXTENDS Naturals, FiniteSets, Sequences

(***************************************************************************
Models the all-or-nothing publication of one bounded suspended-child cohort.

Policy selects the cohort in initd. Loaderd binds the requester to the
kernel-stamped IPC sender. Ring0 snapshots every bounded target
`ProcessIdentity` before acquiring ProcBrokerRegistry. Under that registry
lock it rechecks the full PID/process-generation/MM-generation identity
against the deferred authority, then acquires the Scheduler lock, consumes
every capability while every target remains suspended, and publishes every
runnable task in the same rollback-free critical section. The model exposes
that lock-held interior as `AuthorityConsumed` only to prove the execution
order; no external action, requester exit, dispatch, or reply can interleave
it. A malformed, foreign, or PID-equal replaced cohort changes neither task
nor capability state.

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
AuthorityConsumed == "authority-consumed"
Committed == "committed"
Rejected == "rejected"
Exited == "exited"

Batch == <<BatchFirst, BatchSecond, BatchThird>>
BatchSet == {Batch[index] : index \in 1..Len(Batch)}

GenerationRaceTarget == BatchFirst
SnapshotGeneration == 1
ReplacementGeneration == 2

\* One non-reusable generation represents the exact process-table/MM pair.
\* A replacement retains the numeric PID while substituting that full pair.
TargetIdentity(generation) ==
    [pid |-> GenerationRaceTarget,
     processGeneration |-> generation,
     mmGeneration |-> generation]

VARIABLES taskState, authority, shapeValid, phase, queue, ordinaryQueue,
          dispatched, replyResumed, snapshotGeneration, currentGeneration

vars ==
    <<taskState, authority, shapeValid, phase, queue, ordinaryQueue,
      dispatched, replyResumed, snapshotGeneration, currentGeneration>>

TargetIdentityIsExact ==
    TargetIdentity(currentGeneration) = TargetIdentity(snapshotGeneration)

TargetIdentityReplaced ==
    /\ TargetIdentity(currentGeneration).pid =
        TargetIdentity(snapshotGeneration).pid
    /\ currentGeneration # snapshotGeneration

AllBatchTargetIdentitiesExact == TargetIdentityIsExact

Init ==
    /\ taskState = [task \in Targets |-> Suspended]
    /\ authority = [task \in Targets |-> Live]
    \* Initial resolution completes before the ProcBrokerRegistry critical
    \* section. The authority record binds this exact snapshot generation.
    /\ snapshotGeneration = SnapshotGeneration
    /\ currentGeneration = SnapshotGeneration
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
          dispatched, replyResumed, snapshotGeneration, currentGeneration>>

CorruptShape ==
    /\ phase = Idle
    /\ shapeValid
    /\ shapeValid' = FALSE
    /\ UNCHANGED
        <<taskState, authority, phase, queue, ordinaryQueue,
          dispatched, replyResumed, snapshotGeneration, currentGeneration>>

ReplaceTargetIdentity ==
    /\ phase = Idle
    /\ currentGeneration = snapshotGeneration
    /\ currentGeneration' = ReplacementGeneration
    /\ UNCHANGED
        <<taskState, authority, shapeValid, phase, queue, ordinaryQueue,
          dispatched, replyResumed, snapshotGeneration>>

BatchPreflightOK ==
    /\ shapeValid
    /\ Len(Batch) \in 1..8
    /\ \A left, right \in 1..Len(Batch):
        left # right => Batch[left] # Batch[right]
    /\ TargetIdentityIsExact
    /\ \A task \in BatchSet:
        /\ taskState[task] = Suspended
        /\ authority[task] = Live

ConsumeAuthority ==
    /\ phase = Idle
    /\ BatchPreflightOK
    /\ authority' =
        [task \in Targets |->
            IF task \in BatchSet THEN Consumed ELSE authority[task]]
    /\ phase' = AuthorityConsumed
    /\ UNCHANGED
        <<taskState, shapeValid, queue, ordinaryQueue, dispatched,
          replyResumed, snapshotGeneration, currentGeneration>>

PublishBatch ==
    /\ phase = AuthorityConsumed
    /\ \A task \in BatchSet:
        /\ taskState[task] = Suspended
        /\ authority[task] = Consumed
    /\ taskState' =
        [task \in Targets |->
            IF task \in BatchSet THEN Runnable ELSE taskState[task]]
    /\ phase' = Committed
    /\ queue' = Batch
    /\ dispatched' = <<>>
    /\ replyResumed' = FALSE
    /\ UNCHANGED <<authority, shapeValid, ordinaryQueue,
                   snapshotGeneration, currentGeneration>>

DispatchBatchHead ==
    /\ phase = Committed
    /\ Len(queue) > 0
    /\ queue' = Tail(queue)
    /\ dispatched' = Append(dispatched, Head(queue))
    /\ UNCHANGED
        <<taskState, authority, shapeValid, phase, ordinaryQueue,
          replyResumed, snapshotGeneration, currentGeneration>>

ResumeLoaderReply ==
    /\ phase = Committed
    /\ queue = <<>>
    /\ dispatched = Batch
    /\ ~replyResumed
    /\ replyResumed' = TRUE
    /\ UNCHANGED
        <<taskState, authority, shapeValid, phase, queue, ordinaryQueue,
          dispatched, snapshotGeneration, currentGeneration>>

RejectBatch ==
    /\ phase = Idle
    /\ ~BatchPreflightOK
    /\ phase' = Rejected
    /\ UNCHANGED
        <<taskState, authority, shapeValid, queue, ordinaryQueue,
          dispatched, replyResumed, snapshotGeneration, currentGeneration>>

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
        <<shapeValid, queue, ordinaryQueue, dispatched, replyResumed,
          snapshotGeneration, currentGeneration>>

TerminalStutter ==
    /\ (phase \in {Rejected, Exited} \/ (phase = Committed /\ replyResumed))
    /\ UNCHANGED vars

Next ==
    \/ \E task \in Targets: CorruptAuthority(task)
    \/ CorruptShape
    \/ ReplaceTargetIdentity
    \/ ConsumeAuthority
    \/ PublishBatch
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
    /\ snapshotGeneration = SnapshotGeneration
    /\ currentGeneration \in {SnapshotGeneration, ReplacementGeneration}
    /\ shapeValid \in BOOLEAN
    /\ phase \in {Idle, AuthorityConsumed, Committed, Rejected, Exited}
    /\ queue \in Seq(Targets)
    /\ ordinaryQueue \in Seq({Ordinary})
    /\ dispatched \in Seq(Targets)
    /\ replyResumed \in BOOLEAN

TargetIdentityFieldsStayPidBound ==
    /\ TargetIdentity(snapshotGeneration).pid = GenerationRaceTarget
    /\ TargetIdentity(currentGeneration).pid = GenerationRaceTarget

BatchIsBoundedAndUnique ==
    /\ Len(Batch) \in 1..8
    /\ \A left, right \in 1..Len(Batch):
        left # right => Batch[left] # Batch[right]

NoPartialPublication ==
    phase = Committed =>
        /\ \A task \in BatchSet: taskState[task] = Runnable
        /\ \A task \in BatchSet: authority[task] = Consumed
        /\ AllBatchTargetIdentitiesExact
        /\ dispatched \o queue = Batch

NoRunnablePublicationBeforePublish ==
    phase # Committed =>
        /\ \A task \in BatchSet: taskState[task] # Runnable
        /\ queue = <<>>
        /\ dispatched = <<>>
        /\ ~replyResumed

AuthorityConsumptionRetainsSuspension ==
    phase = AuthorityConsumed =>
        /\ \A task \in BatchSet:
            /\ taskState[task] = Suspended
            /\ authority[task] = Consumed
        /\ AllBatchTargetIdentitiesExact
        /\ queue = <<>>
        /\ dispatched = <<>>
        /\ ~replyResumed

CapabilityConsumptionRequiresCompletePreflight ==
    \A task \in BatchSet:
        authority[task] = Consumed =>
            phase \in {AuthorityConsumed, Committed}

RunnableRequiresConsumedAuthority ==
    \A task \in BatchSet:
        taskState[task] = Runnable => authority[task] = Consumed

ConsumedOrPublishedTargetsMatchResolvedIdentity ==
    phase \in {AuthorityConsumed, Committed} =>
        AllBatchTargetIdentitiesExact

TargetIdentityReplacementCannotActivate ==
    TargetIdentityReplaced =>
        /\ phase \notin {AuthorityConsumed, Committed}
        /\ \A task \in BatchSet: authority[task] # Consumed
        /\ \A task \in BatchSet: taskState[task] # Runnable
        /\ queue = <<>>
        /\ dispatched = <<>>
        /\ ~replyResumed

ReplacementRejectionPreservesCohort ==
    /\ phase = Rejected
    /\ TargetIdentityReplaced
    => /\ \A task \in BatchSet:
            /\ taskState[task] = Suspended
            /\ authority[task] # Consumed
       /\ queue = <<>>
       /\ dispatched = <<>>
       /\ ~replyResumed

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
