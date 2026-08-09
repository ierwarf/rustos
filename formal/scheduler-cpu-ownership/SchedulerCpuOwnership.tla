-------------------- MODULE SchedulerCpuOwnership --------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Models the CPU-affine scheduler/lock contract that sits below task policy.

One admitted CPU publishes one current task. During a context switch it also
retains the outgoing stack as a transition owner until assembly has changed
RSP to the incoming frame. Raw acquisition first owns a pending preemption
unit, then atomically converts it into a held guard. This models an IRQ arriving
while a process-context lock is still spinning without allowing either
acquisition to consume the other's pin. Release removes one held unit and one
total preemption unit on the same CPU. Timer/reschedule IRQs may record work,
but dispatch while any pending or held unit exists fails closed. Foreign
release, underflow, premature outgoing-stack release, or counter disagreement
also fails closed.

Concrete owners:
  * kernel/nucleus-core/src/util/lockdep.rs
  * kernel/ps/src/multitask/{cpu_local,irq}.rs
***************************************************************************)

CONSTANT Cpus, Tasks, NoCpu, NoTask, MaxDepth

VARIABLES current, transitionFrom, taskCpu, ready, blocked, mailbox,
          mailboxCpu, legacyReady, localQueueCpu, runnable, dispatchClaim,
          preemptDepth, pendingDepth, heldDepth, guardTask, panicked

vars == <<current, transitionFrom, taskCpu, ready, blocked, mailbox,
          mailboxCpu, legacyReady, localQueueCpu, runnable, dispatchClaim,
          preemptDepth, pendingDepth, heldDepth, guardTask, panicked>>

ClaimRecord == [task: Tasks \cup {NoTask}, queueCpu: Cpus \cup {NoCpu},
                runnable: BOOLEAN]

Init ==
    /\ current = [cpu \in Cpus |->
        IF cpu = 0 THEN "cpu0-idle" ELSE "cpu1-idle"]
    /\ transitionFrom = [cpu \in Cpus |-> NoTask]
    /\ taskCpu = [task \in Tasks |->
        IF task = "cpu0-idle" THEN 0
        ELSE IF task = "cpu1-idle" THEN 1
        ELSE NoCpu]
    /\ ready = {"worker"}
    /\ blocked = {}
    /\ mailbox = {}
    /\ mailboxCpu = [task \in Tasks |-> NoCpu]
    /\ legacyReady = {"worker"}
    /\ localQueueCpu = [task \in Tasks |->
        IF task = "worker" THEN 0 ELSE NoCpu]
    /\ runnable = [task \in Tasks |-> task = "worker"]
    /\ dispatchClaim = [cpu \in Cpus |->
        [task |-> NoTask, queueCpu |-> NoCpu, runnable |-> FALSE]]
    /\ preemptDepth = [cpu \in Cpus |-> 0]
    /\ pendingDepth = [cpu \in Cpus |-> 0]
    /\ heldDepth = [cpu \in Cpus |-> 0]
    /\ guardTask = [cpu \in Cpus |-> NoTask]
    /\ panicked = FALSE

BeginAcquire(cpu) ==
    /\ ~panicked
    /\ preemptDepth[cpu] < MaxDepth
    /\ transitionFrom[cpu] = NoTask
    /\ guardTask[cpu] \in {NoTask, current[cpu]}
    /\ preemptDepth' = [preemptDepth EXCEPT ![cpu] = @ + 1]
    /\ pendingDepth' = [pendingDepth EXCEPT ![cpu] = @ + 1]
    /\ guardTask' = [guardTask EXCEPT ![cpu] = current[cpu]]
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, blocked, mailbox,
                   mailboxCpu, legacyReady, localQueueCpu, runnable,
                   dispatchClaim, heldDepth, panicked>>

PublishAcquire(cpu) ==
    /\ ~panicked
    /\ pendingDepth[cpu] > 0
    /\ guardTask[cpu] = current[cpu]
    /\ pendingDepth' = [pendingDepth EXCEPT ![cpu] = @ - 1]
    /\ heldDepth' = [heldDepth EXCEPT ![cpu] = @ + 1]
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, blocked, mailbox,
                   mailboxCpu, legacyReady, localQueueCpu, runnable,
                   dispatchClaim, preemptDepth, guardTask, panicked>>

CancelAcquire(cpu) ==
    /\ ~panicked
    /\ pendingDepth[cpu] > 0
    /\ guardTask[cpu] = current[cpu]
    /\ pendingDepth' = [pendingDepth EXCEPT ![cpu] = @ - 1]
    /\ preemptDepth' = [preemptDepth EXCEPT ![cpu] = @ - 1]
    /\ guardTask' = [guardTask EXCEPT
        ![cpu] = IF preemptDepth[cpu] = 1 THEN NoTask ELSE @]
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, blocked, mailbox,
                   mailboxCpu, legacyReady, localQueueCpu, runnable,
                   dispatchClaim, heldDepth, panicked>>

Release(cpu) ==
    /\ ~panicked
    /\ heldDepth[cpu] > 0
    /\ guardTask[cpu] = current[cpu]
    /\ preemptDepth' = [preemptDepth EXCEPT ![cpu] = @ - 1]
    /\ heldDepth' = [heldDepth EXCEPT ![cpu] = @ - 1]
    /\ guardTask' = [guardTask EXCEPT
        ![cpu] = IF preemptDepth[cpu] = 1 THEN NoTask ELSE @]
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, blocked, mailbox,
                   mailboxCpu, legacyReady, localQueueCpu, runnable,
                   dispatchClaim, pendingDepth, panicked>>

StackTransitionCommitted(task) ==
    \A source \in Cpus: transitionFrom[source] # task

BeginDispatch(cpu, nextTask) ==
    /\ ~panicked
    /\ preemptDepth[cpu] = 0
    /\ guardTask[cpu] = NoTask
    /\ transitionFrom[cpu] = NoTask
    /\ nextTask \in ready
    /\ nextTask \notin blocked
    /\ nextTask \notin mailbox
    /\ StackTransitionCommitted(nextTask)
    /\ localQueueCpu[nextTask] = cpu
    /\ runnable[nextTask]
    /\ LET previous == current[cpu] IN
       /\ current' = [current EXCEPT ![cpu] = nextTask]
       /\ transitionFrom' = [transitionFrom EXCEPT ![cpu] = previous]
       /\ taskCpu' = [taskCpu EXCEPT
            ![nextTask] = cpu]
       /\ ready' = ready \ {nextTask}
       /\ legacyReady' = legacyReady \ {nextTask}
       /\ localQueueCpu' = [localQueueCpu EXCEPT ![nextTask] = NoCpu]
       /\ dispatchClaim' = [dispatchClaim EXCEPT ![cpu] =
            [task |-> nextTask, queueCpu |-> localQueueCpu[nextTask],
             runnable |-> runnable[nextTask]]]
    /\ UNCHANGED <<blocked, mailbox, mailboxCpu, runnable,
                   preemptDepth, pendingDepth, heldDepth, guardTask, panicked>>

\* A blocking switch publishes the outgoing stack transition and parks the
\* task under Blocked runqueue custody in the same scheduler transaction.
\* A concurrent wake may then publish remote mailbox custody while the
\* assembly transition still retains the old stack.
BeginBlockedDispatch(cpu, nextTask) ==
    /\ ~panicked
    /\ preemptDepth[cpu] = 0
    /\ guardTask[cpu] = NoTask
    /\ transitionFrom[cpu] = NoTask
    /\ nextTask \in ready
    /\ nextTask \notin blocked
    /\ nextTask \notin mailbox
    /\ StackTransitionCommitted(nextTask)
    /\ localQueueCpu[nextTask] = cpu
    /\ runnable[nextTask]
    /\ LET previous == current[cpu] IN
       /\ previous # "cpu0-idle"
       /\ previous # "cpu1-idle"
       /\ current' = [current EXCEPT ![cpu] = nextTask]
       /\ transitionFrom' = [transitionFrom EXCEPT ![cpu] = previous]
       /\ taskCpu' = [taskCpu EXCEPT ![nextTask] = cpu]
       /\ ready' = ready \ {nextTask}
       /\ legacyReady' = legacyReady \ {nextTask}
       /\ blocked' = blocked \cup {previous}
       /\ localQueueCpu' = [localQueueCpu EXCEPT ![nextTask] = NoCpu]
       /\ runnable' = [runnable EXCEPT ![previous] = FALSE]
       /\ dispatchClaim' = [dispatchClaim EXCEPT ![cpu] =
            [task |-> nextTask, queueCpu |-> localQueueCpu[nextTask],
             runnable |-> runnable[nextTask]]]
    /\ UNCHANGED <<mailbox, mailboxCpu, preemptDepth,
                   pendingDepth, heldDepth, guardTask, panicked>>

CommitStackSwitch(cpu) ==
    /\ ~panicked
    /\ transitionFrom[cpu] # NoTask
    /\ LET previous == transitionFrom[cpu] IN
       /\ transitionFrom' = [transitionFrom EXCEPT ![cpu] = NoTask]
       /\ taskCpu' = [taskCpu EXCEPT ![previous] = NoCpu]
       /\ ready' = IF previous \in (blocked \cup ready)
                    THEN ready ELSE ready \cup {previous}
       /\ localQueueCpu' = IF previous \in (blocked \cup ready)
                            THEN localQueueCpu
                            ELSE [localQueueCpu EXCEPT ![previous] = cpu]
       /\ runnable' = IF previous \in (blocked \cup ready)
                       THEN runnable
                       ELSE [runnable EXCEPT ![previous] = TRUE]
       /\ legacyReady' = IF previous \in blocked
                          THEN legacyReady ELSE legacyReady \cup {previous}
    /\ UNCHANGED <<current, blocked, mailbox, mailboxCpu,
                   dispatchClaim, preemptDepth, pendingDepth, heldDepth,
                   guardTask, panicked>>

\* A remote wake has three ordered visibility points.  The target mailbox is
\* the durable authority; the persistent legacy-ready bit is set only after
\* that custody; only the committed drain makes the task locally dispatchable.
PublishBlockedWake(task, targetCpu) ==
    /\ ~panicked
    /\ task \in blocked
    /\ task \notin mailbox
    /\ mailboxCpu[task] = NoCpu
    /\ mailbox' = mailbox \cup {task}
    /\ mailboxCpu' = [mailboxCpu EXCEPT ![task] = targetCpu]
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, blocked,
                   legacyReady, localQueueCpu, runnable, dispatchClaim,
                   preemptDepth, pendingDepth, heldDepth, guardTask, panicked>>

PublishLegacyReady(task) ==
    /\ ~panicked
    /\ task \in blocked
    /\ task \in mailbox
    /\ mailboxCpu[task] \in Cpus
    /\ task \notin legacyReady
    /\ legacyReady' = legacyReady \cup {task}
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, blocked, mailbox,
                   mailboxCpu, localQueueCpu, runnable, dispatchClaim,
                   preemptDepth, pendingDepth, heldDepth, guardTask, panicked>>

CommitMailboxDrain(task, targetCpu) ==
    /\ ~panicked
    /\ task \in blocked
    /\ task \in mailbox
    /\ mailboxCpu[task] = targetCpu
    /\ task \in legacyReady
    /\ StackTransitionCommitted(task)
    /\ localQueueCpu[task] = NoCpu
    /\ blocked' = blocked \ {task}
    /\ mailbox' = mailbox \ {task}
    /\ mailboxCpu' = [mailboxCpu EXCEPT ![task] = NoCpu]
    /\ ready' = ready \cup {task}
    /\ localQueueCpu' = [localQueueCpu EXCEPT ![task] = targetCpu]
    /\ runnable' = [runnable EXCEPT ![task] = TRUE]
    /\ UNCHANGED <<current, transitionFrom, taskCpu, legacyReady, dispatchClaim,
                   preemptDepth, pendingDepth, heldDepth, guardTask, panicked>>

\* A queued task may retain queue custody while a stop/revoke path makes it
\* non-dispatchable.  This creates the otherwise easy-to-miss state needed to
\* prove that BeginDispatch checks runnable independently of ready membership.
BlockReadyTask(task) ==
    /\ ~panicked
    /\ task \in ready
    /\ runnable[task]
    /\ \A cpu \in Cpus: transitionFrom[cpu] # task
    /\ runnable' = [runnable EXCEPT ![task] = FALSE]
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, blocked, mailbox,
                   mailboxCpu, legacyReady, localQueueCpu, dispatchClaim,
                   preemptDepth, pendingDepth, heldDepth, guardTask, panicked>>

WakeReadyTask(task) ==
    /\ ~panicked
    /\ task \in ready
    /\ ~runnable[task]
    /\ runnable' = [runnable EXCEPT ![task] = TRUE]
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, blocked, mailbox,
                   mailboxCpu, legacyReady, localQueueCpu, dispatchClaim,
                   preemptDepth, pendingDepth, heldDepth, guardTask, panicked>>

AttemptDispatchWhileGuarded(cpu) ==
    /\ ~panicked
    /\ preemptDepth[cpu] > 0
    /\ panicked' = TRUE
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, blocked, mailbox,
                   mailboxCpu, legacyReady, localQueueCpu, runnable,
                   dispatchClaim, preemptDepth, pendingDepth, heldDepth,
                   guardTask>>

AttemptWrongCpuRelease(ownerCpu, releaseCpu) ==
    /\ ~panicked
    /\ ownerCpu # releaseCpu
    /\ heldDepth[ownerCpu] > 0
    /\ guardTask[ownerCpu] # NoTask
    /\ panicked' = TRUE
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, blocked, mailbox,
                   mailboxCpu, legacyReady, localQueueCpu, runnable,
                   dispatchClaim, preemptDepth, pendingDepth, heldDepth,
                   guardTask>>

AttemptUnderflow(cpu) ==
    /\ ~panicked
    /\ preemptDepth[cpu] = 0
    /\ panicked' = TRUE
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, blocked, mailbox,
                   mailboxCpu, legacyReady, localQueueCpu, runnable,
                   dispatchClaim, preemptDepth, pendingDepth, heldDepth,
                   guardTask>>

PanickedStutter ==
    /\ panicked
    /\ UNCHANGED vars

Next ==
    \/ \E cpu \in Cpus:
        \/ BeginAcquire(cpu)
        \/ PublishAcquire(cpu)
        \/ CancelAcquire(cpu)
        \/ Release(cpu)
        \/ AttemptDispatchWhileGuarded(cpu)
        \/ AttemptUnderflow(cpu)
    \/ \E cpu \in Cpus, task \in Tasks:
        \/ BeginDispatch(cpu, task)
        \/ BeginBlockedDispatch(cpu, task)
    \/ \E cpu \in Cpus: CommitStackSwitch(cpu)
    \/ \E task \in Tasks, targetCpu \in Cpus:
        PublishBlockedWake(task, targetCpu)
    \/ \E task \in Tasks: PublishLegacyReady(task)
    \/ \E task \in Tasks, targetCpu \in Cpus:
        CommitMailboxDrain(task, targetCpu)
    \/ \E task \in Tasks: BlockReadyTask(task)
    \/ \E task \in Tasks: WakeReadyTask(task)
    \/ \E owner \in Cpus, release \in Cpus:
        AttemptWrongCpuRelease(owner, release)
    \/ PanickedStutter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ current \in [Cpus -> Tasks]
    /\ transitionFrom \in [Cpus -> (Tasks \cup {NoTask})]
    /\ taskCpu \in [Tasks -> (Cpus \cup {NoCpu})]
    /\ ready \subseteq Tasks
    /\ blocked \subseteq Tasks
    /\ mailbox \subseteq Tasks
    /\ mailboxCpu \in [Tasks -> (Cpus \cup {NoCpu})]
    /\ legacyReady \subseteq Tasks
    /\ localQueueCpu \in [Tasks -> (Cpus \cup {NoCpu})]
    /\ runnable \in [Tasks -> BOOLEAN]
    /\ dispatchClaim \in [Cpus -> ClaimRecord]
    /\ preemptDepth \in [Cpus -> (0..MaxDepth)]
    /\ pendingDepth \in [Cpus -> (0..MaxDepth)]
    /\ heldDepth \in [Cpus -> (0..MaxDepth)]
    /\ guardTask \in [Cpus -> (Tasks \cup {NoTask})]
    /\ panicked \in BOOLEAN

CurrentTasksAreUnique ==
    \A left, right \in Cpus:
        left # right => current[left] # current[right]

CurrentMatchesTaskCpu ==
    \A cpu \in Cpus: taskCpu[current[cpu]] = cpu

TransitionMatchesTaskCpu ==
    \A cpu \in Cpus:
        transitionFrom[cpu] # NoTask =>
            /\ transitionFrom[cpu] # current[cpu]
            /\ taskCpu[transitionFrom[cpu]] = cpu

PublishedOwnersAreUnique ==
    \A task \in Tasks:
        Cardinality({cpu \in Cpus:
            current[cpu] = task \/ transitionFrom[cpu] = task}) <= 1

ReadyTasksAreNotRunning ==
    \A task \in ready:
        /\ \A cpu \in Cpus: current[cpu] # task
        /\ taskCpu[task] = NoCpu

ReadyTasksHaveLocalQueueCustody ==
    \A task \in ready:
        /\ localQueueCpu[task] \in Cpus
        /\ task \in legacyReady

RunningTasksHaveNoQueueCustody ==
    \A cpu \in Cpus: localQueueCpu[current[cpu]] = NoCpu

BlockedTasksRetainNoLocalQueueCustody ==
    \A task \in blocked:
        /\ task \notin ready
        /\ localQueueCpu[task] = NoCpu
        /\ ~runnable[task]

MailboxCpuMatchesPublishedMailbox ==
    \A task \in Tasks:
        (mailboxCpu[task] \in Cpus) = (task \in mailbox)

MailboxTasksHaveRemoteCustody ==
    \A task \in mailbox:
        /\ task \in blocked
        /\ task \notin ready
        /\ mailboxCpu[task] \in Cpus
        /\ localQueueCpu[task] = NoCpu
        /\ ~runnable[task]

LegacyReadyHasPublishedCustody ==
    \A task \in legacyReady:
        \/ /\ task \in mailbox
           /\ mailboxCpu[task] \in Cpus
           /\ task \in blocked
           /\ localQueueCpu[task] = NoCpu
        \/ /\ task \in ready
           /\ localQueueCpu[task] \in Cpus

TransitionQueueCustodyIsExplicit ==
    \A cpu \in Cpus:
        transitionFrom[cpu] # NoTask =>
            LET task == transitionFrom[cpu] IN
            /\ IF task \in mailbox
                  THEN /\ mailboxCpu[task] \in Cpus
                       /\ localQueueCpu[task] = NoCpu
                       /\ ~runnable[task]
                  ELSE /\ task \notin ready
                       /\ localQueueCpu[task] = NoCpu

RecordedDispatchClaimIsLocalExactRunnable ==
    \A cpu \in Cpus:
        dispatchClaim[cpu].task # NoTask =>
            /\ dispatchClaim[cpu].queueCpu = cpu
            /\ dispatchClaim[cpu].runnable

GuardPinsExactCurrentTask ==
    \A cpu \in Cpus:
        guardTask[cpu] # NoTask => guardTask[cpu] = current[cpu]

DepthAndGuardAgree ==
    \A cpu \in Cpus:
        (preemptDepth[cpu] = 0) = (guardTask[cpu] = NoTask)

PreemptionUnitsAreExact ==
    \A cpu \in Cpus:
        preemptDepth[cpu] = pendingDepth[cpu] + heldDepth[cpu]

EveryTaskHasOneExecutionAuthority ==
    \A task \in Tasks:
        LET CurrentOwners == {cpu \in Cpus: current[cpu] = task}
            TransitionOwners == {cpu \in Cpus: transitionFrom[cpu] = task}
            InTransition == Cardinality(TransitionOwners) = 1
            InMailbox == task \in mailbox
            TransitionOwner == IF InTransition /\ ~InMailbox
                               THEN Cardinality(TransitionOwners) ELSE 0
            MailboxOwner == IF InMailbox THEN 1 ELSE 0
            ReadyOwner == IF task \in ready /\ ~InTransition /\ ~InMailbox
                          THEN 1 ELSE 0
            BlockedOwner == IF task \in blocked /\ ~InTransition /\ ~InMailbox
                            THEN 1 ELSE 0
        IN Cardinality(CurrentOwners)
             + TransitionOwner
             + MailboxOwner
             + ReadyOwner
             + BlockedOwner = 1

TransitionTaskCannotDispatch ==
    \A cpu \in Cpus:
        transitionFrom[cpu] # NoTask =>
            \A runningCpu \in Cpus: current[runningCpu] # transitionFrom[cpu]

GuardedCpuHasNoStackTransition ==
    \A cpu \in Cpus:
        preemptDepth[cpu] > 0 => transitionFrom[cpu] = NoTask

=============================================================================
