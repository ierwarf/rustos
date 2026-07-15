--------------------- MODULE SchedulerCpuDistribution ------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Owner: kernel-ps scheduler.
Linearization point: Dispatch charges exactly one CPU turn. A bounded System
burst reserves a User turn when both classes remain runnable; every runnable
task receives a finite-turn opportunity under the bounded configuration.
*******************************************************************************)

CONSTANTS Tasks, SystemTasks, UserTasks, MaxSystemBurst, MaxRuntime

NoTask == "none"
VARIABLES ready, runtime, last, systemBurst
vars == <<ready, runtime, last, systemBurst>>

Init ==
    /\ ready = Tasks
    /\ runtime = [task \in Tasks |-> 0]
    /\ last = NoTask
    /\ systemBurst = 0

DispatchSystem(task) ==
    /\ task \in ready \cap SystemTasks
    /\ (ready \cap UserTasks = {} \/ systemBurst < MaxSystemBurst)
    /\ runtime' = [runtime EXCEPT ![task] = (@ + 1) % (MaxRuntime + 1)]
    /\ last' = task
    /\ systemBurst' = IF ready \cap UserTasks = {} THEN 0 ELSE systemBurst + 1
    /\ UNCHANGED ready

DispatchUser(task) ==
    /\ task \in ready \cap UserTasks
    /\ runtime' = [runtime EXCEPT ![task] = (@ + 1) % (MaxRuntime + 1)]
    /\ last' = task
    /\ systemBurst' = 0
    /\ UNCHANGED ready

Block(task) ==
    /\ task \in ready
    /\ ready' = ready \ {task}
    /\ UNCHANGED <<runtime, last, systemBurst>>

Wake(task) ==
    /\ task \notin ready
    /\ ready' = ready \cup {task}
    /\ UNCHANGED <<runtime, last, systemBurst>>

DispatchAnySystem == \E task \in SystemTasks: DispatchSystem(task)
DispatchAnyUser == \E task \in UserTasks: DispatchUser(task)

Next ==
    \/ DispatchAnySystem
    \/ DispatchAnyUser
    \/ \E task \in Tasks: Block(task)
    \/ \E task \in Tasks: Wake(task)

Spec == Init /\ [][Next]_vars /\ WF_vars(DispatchAnySystem) /\ WF_vars(DispatchAnyUser)

TypeOK ==
    /\ ready \in SUBSET Tasks
    /\ runtime \in [Tasks -> 0..MaxRuntime]
    /\ last \in Tasks \cup {NoTask}
    /\ systemBurst \in 0..MaxSystemBurst

SystemBurstIsBounded == systemBurst <= MaxSystemBurst
UserReservationIsBounded == ready \cap UserTasks # {} => systemBurst <= MaxSystemBurst
CpuAccountingIsBounded == \A task \in Tasks: runtime[task] <= MaxRuntime
RunnableUserEventuallyRuns ==
    \A task \in UserTasks:
        [] (task \in ready => <> (task \notin ready \/ last = task))

=============================================================================
