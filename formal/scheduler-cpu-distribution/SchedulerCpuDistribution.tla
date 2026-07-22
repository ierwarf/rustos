--------------------- MODULE SchedulerCpuDistribution ------------------------
EXTENDS Naturals, FiniteSets, Sequences

(*******************************************************************************
Owner: kernel-ps scheduler.
Linearization point: Dispatch charges exactly one CPU turn. A bounded System
burst reserves a User turn when both classes remain runnable, while a per-task
ready-age deadline prevents several User tasks from sharing one reservation so
coarsely that an individual application misses every compositor frame.
*******************************************************************************)

CONSTANTS Tasks, SystemTasks, UserTasks, MaxSystemBurst, MaxRuntime, MaxUserWait,
          MaxLatencyBurst, MaxLatencyHints

NoTask == "none"
VARIABLES ready, runtime, readyAge, last, systemBurst, latencyBurst, latencyHints
vars == <<ready, runtime, readyAge, last, systemBurst, latencyBurst, latencyHints>>

Init ==
    /\ ready = Tasks
    /\ runtime = [task \in Tasks |-> 0]
    /\ readyAge = [task \in Tasks |-> 0]
    /\ last = NoTask
    /\ systemBurst = 0
    /\ latencyBurst = 0
    /\ latencyHints = <<>>

OverdueUsers == {task \in ready \cap UserTasks: readyAge[task] = MaxUserWait}

AdvanceReadyAge(dispatched) ==
    [task \in Tasks |->
        IF task = dispatched \/ task \notin ready
        THEN 0
        ELSE IF readyAge[task] < MaxUserWait
             THEN readyAge[task] + 1
             ELSE MaxUserWait]

DispatchSystem(task) ==
    /\ task \in ready \cap SystemTasks
    /\ (ready \cap UserTasks = {} \/ systemBurst < MaxSystemBurst)
    /\ OverdueUsers = {}
    /\ (latencyHints = <<>> \/ latencyBurst = MaxLatencyBurst)
    /\ runtime' = [runtime EXCEPT ![task] = (@ + 1) % (MaxRuntime + 1)]
    /\ readyAge' = AdvanceReadyAge(task)
    /\ last' = task
    /\ systemBurst' = IF ready \cap UserTasks = {} THEN 0 ELSE systemBurst + 1
    /\ latencyBurst' = 0
    /\ UNCHANGED <<ready, latencyHints>>

DispatchUser(task) ==
    /\ task \in ready \cap UserTasks
    /\ (OverdueUsers = {} \/ task \in OverdueUsers)
    /\ (latencyHints = <<>> \/ latencyBurst = MaxLatencyBurst)
    /\ runtime' = [runtime EXCEPT ![task] = (@ + 1) % (MaxRuntime + 1)]
    /\ readyAge' = AdvanceReadyAge(task)
    /\ last' = task
    /\ systemBurst' = 0
    /\ latencyBurst' = 0
    /\ UNCHANGED <<ready, latencyHints>>

DispatchLatency ==
    /\ latencyHints # <<>>
    /\ latencyBurst < MaxLatencyBurst
    /\ Head(latencyHints) \in ready \cap UserTasks
    /\ LET task == Head(latencyHints) IN
       /\ runtime' = [runtime EXCEPT ![task] = (@ + 1) % (MaxRuntime + 1)]
       /\ readyAge' = AdvanceReadyAge(task)
       /\ last' = task
    /\ systemBurst' = 0
    /\ latencyBurst' = latencyBurst + 1
    /\ latencyHints' = Tail(latencyHints)
    /\ UNCHANGED ready

QueueLatencyHint(task) ==
    /\ task \in ready \cap UserTasks
    /\ task \notin {latencyHints[index]: index \in DOMAIN latencyHints}
    /\ Len(latencyHints) < MaxLatencyHints
    /\ latencyHints' = Append(latencyHints, task)
    /\ UNCHANGED <<ready, runtime, readyAge, last, systemBurst, latencyBurst>>

DropStaleLatencyHint ==
    /\ latencyHints # <<>>
    /\ Head(latencyHints) \notin ready
    /\ latencyHints' = Tail(latencyHints)
    /\ UNCHANGED <<ready, runtime, readyAge, last, systemBurst, latencyBurst>>

Block(task) ==
    /\ task \in ready
    /\ ready' = ready \ {task}
    /\ readyAge' = [readyAge EXCEPT ![task] = 0]
    \* Keep queued hints until the consumer observes that their owner is no
    \* longer runnable.  Eagerly filtering here made DropStaleLatencyHint
    \* unreachable and failed to model the producer/consumer race.
    /\ UNCHANGED <<runtime, last, systemBurst, latencyBurst, latencyHints>>

Wake(task) ==
    /\ task \notin ready
    /\ ready' = ready \cup {task}
    /\ readyAge' = [readyAge EXCEPT ![task] = 0]
    /\ UNCHANGED <<runtime, last, systemBurst, latencyBurst, latencyHints>>

DispatchAnySystem == \E task \in SystemTasks: DispatchSystem(task)
DispatchAnyUser == \E task \in UserTasks: DispatchUser(task)

Next ==
    \/ DispatchAnySystem
    \/ DispatchAnyUser
    \/ DispatchLatency
    \/ \E task \in UserTasks: QueueLatencyHint(task)
    \/ DropStaleLatencyHint
    \/ \E task \in Tasks: Block(task)
    \/ \E task \in Tasks: Wake(task)

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(DispatchAnySystem)
    /\ \A task \in UserTasks: SF_vars(DispatchUser(task))
    \* A producer can repeatedly make the queue head runnable/unrunnable.
    \* Weak fairness permits that flicker to starve every later hint forever;
    \* the consumer polling contract therefore requires strong fairness for
    \* both consuming a runnable head and dropping an observed stale head.
    /\ SF_vars(DispatchLatency)
    /\ SF_vars(DropStaleLatencyHint)

TypeOK ==
    /\ ready \in SUBSET Tasks
    /\ runtime \in [Tasks -> 0..MaxRuntime]
    /\ readyAge \in [Tasks -> 0..MaxUserWait]
    /\ last \in Tasks \cup {NoTask}
    /\ systemBurst \in 0..MaxSystemBurst
    /\ latencyBurst \in 0..MaxLatencyBurst
    /\ latencyHints \in Seq(UserTasks)
    /\ Len(latencyHints) <= MaxLatencyHints
    /\ \A i, j \in DOMAIN latencyHints:
           i # j => latencyHints[i] # latencyHints[j]

SystemBurstIsBounded == systemBurst <= MaxSystemBurst
UserReservationIsBounded == ready \cap UserTasks # {} => systemBurst <= MaxSystemBurst
CpuAccountingIsBounded == \A task \in Tasks: runtime[task] <= MaxRuntime
UserReadyAgeIsBounded == \A task \in UserTasks: readyAge[task] <= MaxUserWait
LatencyBurstIsBounded == latencyBurst <= MaxLatencyBurst
LatencyHintQueueIsBounded == Len(latencyHints) <= MaxLatencyHints
OverdueUserBlocksSystem == [] (OverdueUsers # {} => ~ENABLED DispatchAnySystem)
RunnableUserEventuallyRuns ==
    \A task \in UserTasks:
        [] (task \in ready => <> (task \notin ready \/ last = task))

=============================================================================
