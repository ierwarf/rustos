---------------------- MODULE SchedulerActiveBalance ------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Owner: kernel-ps scheduler active balance.

This finite model isolates the source-push path in runqueue_policy.rs.  A
source-local loaded-opportunity counter is a residue in 0..7: residue zero is
due before the first loaded opportunity and again after each eight later
loaded opportunities.  It is deliberately not a timer or RTC residue.

The one transferable continuation must be queued on the exact source, runnable,
not execution- or transition-owned, and affinity-valid for the selected target.
The source CAS is represented by removing the source queue custody while
publishing one exact target mailbox and reschedule request.  Only a matching
target drain can make that continuation target-local, and only that drained
custody can dispatch it.

The finite target is the exact result of the source policy's selected target;
this model proves transfer custody, not the separate least-loaded target search.
*******************************************************************************)

CONSTANTS Tasks, MaxOpportunities

Candidate == "candidate"
SourcePeerA == "source-peer-a"
SourcePeerB == "source-peer-b"
Executing == "executing"
Transitioning == "transitioning"
TaskSet == {Candidate, SourcePeerA, SourcePeerB, Executing, Transitioning}

Source == "source"
Target == "target"
WrongTarget == "wrong-target"
Cpus == {Source, Target, WrongTarget}
NoCpu == "no-cpu"
NoTask == "no-task"

VARIABLES sourceQueue, mailbox, requestTarget, targetQueue, running,
          transitioning, runnable, opportunity, transferOpportunity,
          transferred, mailboxDrained

vars == <<sourceQueue, mailbox, requestTarget, targetQueue, running,
          transitioning, runnable, opportunity, transferOpportunity,
          transferred, mailboxDrained>>

Affinity(task) ==
    IF task \in {Candidate, Executing, Transitioning}
    THEN {Source, Target}
    ELSE {Source}

ExecutionOwned(task) == \E cpu \in Cpus: running[cpu] = task
SourceOwned(task) ==
    \/ task \in sourceQueue
    \/ running[Source] = task
    \/ task \in transitioning

QueueOnlyUnowned(task) ==
    /\ task \in sourceQueue
    /\ ~ExecutionOwned(task)
    /\ task \notin transitioning

SourceRunnableCount == Cardinality(sourceQueue \cap runnable)
TargetRunnableCount == Cardinality(targetQueue \cap runnable)
SourceLoaded == SourceRunnableCount >= 2
Imbalanced == SourceRunnableCount > TargetRunnableCount + 1
Due == opportunity = 0

Migratable(task) ==
    /\ QueueOnlyUnowned(task)
    /\ task \in runnable
    /\ Target \in Affinity(task)
    /\ SourceLoaded
    /\ Imbalanced

NextOpportunity ==
    IF opportunity = MaxOpportunities - 1 THEN 0 ELSE opportunity + 1

Init ==
    /\ Tasks = TaskSet
    /\ MaxOpportunities = 8
    /\ sourceQueue = {Candidate, SourcePeerA, SourcePeerB}
    /\ mailbox = [task \in Tasks |-> NoCpu]
    /\ requestTarget = [task \in Tasks |-> NoCpu]
    /\ targetQueue = {}
    /\ running = [cpu \in Cpus |-> IF cpu = Source THEN Executing ELSE NoTask]
    /\ transitioning = {Transitioning}
    /\ runnable = {SourcePeerA, SourcePeerB, Executing, Transitioning}
    /\ opportunity = 0
    /\ transferOpportunity = [task \in Tasks |-> 0]
    /\ transferred = {}
    /\ mailboxDrained = {}

EnableCandidate ==
    /\ Candidate \notin runnable
    /\ runnable' = runnable \cup {Candidate}
    /\ UNCHANGED <<sourceQueue, mailbox, requestTarget, targetQueue, running,
                    transitioning, opportunity, transferOpportunity, transferred,
                    mailboxDrained>>

AdvanceLoadedOpportunity ==
    /\ SourceLoaded
    /\ ~(Due /\ \E task \in Tasks: Migratable(task))
    /\ opportunity' = NextOpportunity
    /\ UNCHANGED <<sourceQueue, mailbox, requestTarget, targetQueue, running,
                    transitioning, runnable, transferOpportunity, transferred,
                    mailboxDrained>>

TransferOne(task) ==
    /\ Due
    /\ Migratable(task)
    /\ sourceQueue' = sourceQueue \ {task}
    /\ mailbox' = [mailbox EXCEPT ![task] = Target]
    /\ requestTarget' = [requestTarget EXCEPT ![task] = Target]
    /\ targetQueue' = targetQueue
    /\ transferOpportunity' = [transferOpportunity EXCEPT ![task] = opportunity]
    /\ transferred' = transferred \cup {task}
    /\ UNCHANGED <<running, transitioning, runnable, opportunity, mailboxDrained>>

DrainTargetMailbox(task) ==
    /\ mailbox[task] = Target
    /\ requestTarget[task] = Target
    /\ task \notin targetQueue
    /\ mailbox' = [mailbox EXCEPT ![task] = NoCpu]
    /\ requestTarget' = [requestTarget EXCEPT ![task] = NoCpu]
    /\ targetQueue' = targetQueue \cup {task}
    /\ mailboxDrained' = mailboxDrained \cup {task}
    /\ UNCHANGED <<sourceQueue, running, transitioning, runnable, opportunity,
                    transferOpportunity, transferred>>

DispatchTarget(task) ==
    /\ task \in targetQueue
    /\ task \in runnable
    /\ task \in mailboxDrained
    /\ running[Target] = NoTask
    /\ targetQueue' = targetQueue \ {task}
    /\ running' = [running EXCEPT ![Target] = task]
    /\ UNCHANGED <<sourceQueue, mailbox, requestTarget, transitioning, runnable,
                    opportunity, transferOpportunity, transferred, mailboxDrained>>

Next ==
    \/ EnableCandidate
    \/ AdvanceLoadedOpportunity
    \/ \E task \in Tasks: TransferOne(task)
    \/ \E task \in Tasks: DrainTargetMailbox(task)
    \/ \E task \in Tasks: DispatchTarget(task)

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(EnableCandidate)
    /\ WF_vars(AdvanceLoadedOpportunity)
    /\ \A task \in Tasks: WF_vars(TransferOne(task))
    /\ \A task \in Tasks: WF_vars(DrainTargetMailbox(task))
    /\ \A task \in Tasks: WF_vars(DispatchTarget(task))

RunningCustodies(task) ==
    {<<"running", cpu>> : cpu \in {cpu \in Cpus: running[cpu] = task}}

TaskCustodies(task) ==
    (IF task \in sourceQueue THEN {<<"source-local", Source>>} ELSE {})
    \cup (IF mailbox[task] # NoCpu THEN {<<"mailbox", mailbox[task]>>} ELSE {})
    \cup (IF task \in targetQueue THEN {<<"target-local", Target>>} ELSE {})
    \cup RunningCustodies(task)
    \cup (IF task \in transitioning THEN {<<"transition", Source>>} ELSE {})

TypeOK ==
    /\ Tasks = TaskSet
    /\ MaxOpportunities = 8
    /\ sourceQueue \in SUBSET Tasks
    /\ mailbox \in [Tasks -> Cpus \cup {NoCpu}]
    /\ requestTarget \in [Tasks -> Cpus \cup {NoCpu}]
    /\ targetQueue \in SUBSET Tasks
    /\ running \in [Cpus -> Tasks \cup {NoTask}]
    /\ transitioning \in SUBSET Tasks
    /\ runnable \in SUBSET Tasks
    /\ opportunity \in 0..(MaxOpportunities - 1)
    /\ transferOpportunity \in [Tasks -> 0..(MaxOpportunities - 1)]
    /\ transferred \in SUBSET Tasks
    /\ mailboxDrained \in SUBSET Tasks

EachTaskHasExactlyOneCustody ==
    \A task \in Tasks: Cardinality(TaskCustodies(task)) = 1

RemoteMailboxHasExactRequest ==
    \A task \in Tasks:
        mailbox[task] # NoCpu => requestTarget[task] = mailbox[task]

RemoteCustodyUsesEligibleTarget ==
    /\ \A task \in Tasks:
        mailbox[task] # NoCpu =>
            /\ mailbox[task] = Target
            /\ Target \in Affinity(task)
    /\ \A task \in Tasks:
        requestTarget[task] # NoCpu => requestTarget[task] = Target
    /\ \A task \in targetQueue: Target \in Affinity(task)

ProtectedExecutionAndTransitionStayAtSource ==
    \A task \in {Executing, Transitioning}:
        /\ mailbox[task] = NoCpu
        /\ requestTarget[task] = NoCpu
        /\ task \notin targetQueue
        /\ running[Target] # task

EveryTransferOccursOnDueOpportunity ==
    \A task \in Tasks:
        task \in transferred => transferOpportunity[task] = 0

TargetDispatchRequiresMailboxDrain ==
    running[Target] # NoTask => running[Target] \in mailboxDrained

ContinuouslyEligibleQueuedCandidateEventuallyRuns ==
    [] (Migratable(Candidate) => <> (running[Target] = Candidate))

=============================================================================
