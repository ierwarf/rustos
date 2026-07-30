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

VARIABLES current, transitionFrom, taskCpu, ready, preemptDepth, pendingDepth,
          heldDepth, guardTask, panicked

vars == <<current, transitionFrom, taskCpu, ready, preemptDepth, pendingDepth,
          heldDepth, guardTask, panicked>>

Init ==
    /\ current = [cpu \in Cpus |->
        IF cpu = 0 THEN "cpu0-idle" ELSE "cpu1-idle"]
    /\ transitionFrom = [cpu \in Cpus |-> NoTask]
    /\ taskCpu = [task \in Tasks |->
        IF task = "cpu0-idle" THEN 0
        ELSE IF task = "cpu1-idle" THEN 1
        ELSE NoCpu]
    /\ ready = {"worker"}
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
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, heldDepth, panicked>>

PublishAcquire(cpu) ==
    /\ ~panicked
    /\ pendingDepth[cpu] > 0
    /\ guardTask[cpu] = current[cpu]
    /\ pendingDepth' = [pendingDepth EXCEPT ![cpu] = @ - 1]
    /\ heldDepth' = [heldDepth EXCEPT ![cpu] = @ + 1]
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, preemptDepth,
                   guardTask, panicked>>

CancelAcquire(cpu) ==
    /\ ~panicked
    /\ pendingDepth[cpu] > 0
    /\ guardTask[cpu] = current[cpu]
    /\ pendingDepth' = [pendingDepth EXCEPT ![cpu] = @ - 1]
    /\ preemptDepth' = [preemptDepth EXCEPT ![cpu] = @ - 1]
    /\ guardTask' = [guardTask EXCEPT
        ![cpu] = IF preemptDepth[cpu] = 1 THEN NoTask ELSE @]
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, heldDepth, panicked>>

Release(cpu) ==
    /\ ~panicked
    /\ heldDepth[cpu] > 0
    /\ guardTask[cpu] = current[cpu]
    /\ preemptDepth' = [preemptDepth EXCEPT ![cpu] = @ - 1]
    /\ heldDepth' = [heldDepth EXCEPT ![cpu] = @ - 1]
    /\ guardTask' = [guardTask EXCEPT
        ![cpu] = IF preemptDepth[cpu] = 1 THEN NoTask ELSE @]
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, pendingDepth, panicked>>

BeginDispatch(cpu, nextTask) ==
    /\ ~panicked
    /\ preemptDepth[cpu] = 0
    /\ guardTask[cpu] = NoTask
    /\ transitionFrom[cpu] = NoTask
    /\ nextTask \in ready
    /\ LET previous == current[cpu] IN
       /\ current' = [current EXCEPT ![cpu] = nextTask]
       /\ transitionFrom' = [transitionFrom EXCEPT ![cpu] = previous]
       /\ taskCpu' = [taskCpu EXCEPT
            ![nextTask] = cpu]
       /\ ready' = ready \ {nextTask}
    /\ UNCHANGED <<preemptDepth, pendingDepth, heldDepth, guardTask, panicked>>

CommitStackSwitch(cpu) ==
    /\ ~panicked
    /\ transitionFrom[cpu] # NoTask
    /\ LET previous == transitionFrom[cpu] IN
       /\ transitionFrom' = [transitionFrom EXCEPT ![cpu] = NoTask]
       /\ taskCpu' = [taskCpu EXCEPT ![previous] = NoCpu]
       /\ ready' = ready \cup {previous}
    /\ UNCHANGED <<current, preemptDepth, pendingDepth, heldDepth,
                   guardTask, panicked>>

AttemptDispatchWhileGuarded(cpu) ==
    /\ ~panicked
    /\ preemptDepth[cpu] > 0
    /\ panicked' = TRUE
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, preemptDepth,
                   pendingDepth, heldDepth, guardTask>>

AttemptWrongCpuRelease(ownerCpu, releaseCpu) ==
    /\ ~panicked
    /\ ownerCpu # releaseCpu
    /\ heldDepth[ownerCpu] > 0
    /\ guardTask[ownerCpu] # NoTask
    /\ panicked' = TRUE
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, preemptDepth,
                   pendingDepth, heldDepth, guardTask>>

AttemptUnderflow(cpu) ==
    /\ ~panicked
    /\ preemptDepth[cpu] = 0
    /\ panicked' = TRUE
    /\ UNCHANGED <<current, transitionFrom, taskCpu, ready, preemptDepth,
                   pendingDepth, heldDepth, guardTask>>

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
    \/ \E cpu \in Cpus, task \in Tasks: BeginDispatch(cpu, task)
    \/ \E cpu \in Cpus: CommitStackSwitch(cpu)
    \/ \E owner \in Cpus, release \in Cpus:
        AttemptWrongCpuRelease(owner, release)
    \/ PanickedStutter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ current \in [Cpus -> Tasks]
    /\ transitionFrom \in [Cpus -> (Tasks \cup {NoTask})]
    /\ taskCpu \in [Tasks -> (Cpus \cup {NoCpu})]
    /\ ready \subseteq Tasks
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
    \A task \in ready: taskCpu[task] = NoCpu

GuardPinsExactCurrentTask ==
    \A cpu \in Cpus:
        guardTask[cpu] # NoTask => guardTask[cpu] = current[cpu]

DepthAndGuardAgree ==
    \A cpu \in Cpus:
        (preemptDepth[cpu] = 0) = (guardTask[cpu] = NoTask)

PreemptionUnitsAreExact ==
    \A cpu \in Cpus:
        preemptDepth[cpu] = pendingDepth[cpu] + heldDepth[cpu]

=============================================================================
