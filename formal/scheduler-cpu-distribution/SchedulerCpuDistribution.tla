--------------------- MODULE SchedulerCpuDistribution ------------------------
EXTENDS Naturals, FiniteSets, Sequences

(*******************************************************************************
Owner: kernel-ps scheduler.
Linearization point: Dispatch charges exactly one CPU turn. A bounded System
burst reserves a User turn when both classes remain runnable. User selection
uses charged virtual runtime; a wall-clock deadline without bandwidth admission
would be unsatisfiable under overload and must not bypass fair-share weights.
Exact wake and synchronous IPC urgency live in separately bounded handoffs.
Ordinary picks may retain the last CPU only within a finite virtual-runtime
lag; locality cannot cross class, affinity, or fairness admission.
*******************************************************************************)

CONSTANTS Tasks, SystemTasks, UserTasks, MaxSystemBurst, MaxRuntime, MaxSystemWait,
          MaxLatencyBurst, MaxLatencyHints, Cpus, MaxLocalityLag

NoTask == "none"
NoCpu == "no-cpu"
VARIABLES ready, runtime, readyAge, last, lastCpu, systemBurst, latencyBurst,
          latencyHints, fairUserPick
vars == <<ready, runtime, readyAge, last, lastCpu, systemBurst, latencyBurst,
          latencyHints, fairUserPick>>

Init ==
    /\ ready = Tasks
    /\ runtime = [task \in Tasks |-> 0]
    /\ readyAge = [task \in Tasks |-> 0]
    /\ last = NoTask
    /\ lastCpu = [task \in Tasks |-> NoCpu]
    /\ systemBurst = 0
    /\ latencyBurst = 0
    /\ latencyHints = <<>>
    /\ fairUserPick = TRUE

OverdueSystems == {task \in ready \cap SystemTasks: readyAge[task] = MaxSystemWait}

LeastRuntimeIn(task, classTasks) ==
    \A peer \in ready \cap classTasks : runtime[task] <= runtime[peer]

LocalityAdmissible(task, cpu, classTasks) ==
    /\ lastCpu[task] = cpu
    /\ \A peer \in ready \cap classTasks:
           runtime[task] <= runtime[peer] + MaxLocalityLag

LocalityWithinBound(task, cpu, classTasks) ==
    /\ lastCpu[task] = cpu
    /\ \A peer \in ready \cap classTasks:
           runtime[task] <= runtime[peer] + MaxLocalityLag

AdmittedFairPick(task, cpu, classTasks) ==
    LeastRuntimeIn(task, classTasks) \/ LocalityAdmissible(task, cpu, classTasks)

ObservedFairPick(task, cpu, classTasks) ==
    LeastRuntimeIn(task, classTasks) \/ LocalityWithinBound(task, cpu, classTasks)

AdvanceReadyAge(dispatched) ==
    [task \in Tasks |->
        IF task = dispatched \/ task \notin ready \/ task \notin SystemTasks
        THEN 0
        ELSE IF readyAge[task] < MaxSystemWait
             THEN readyAge[task] + 1
             ELSE MaxSystemWait]

ChargeRuntime(task) ==
    IF runtime[task] < MaxRuntime THEN runtime[task] + 1 ELSE MaxRuntime

DispatchSystem(task, cpu) ==
    /\ task \in ready \cap SystemTasks
    /\ cpu \in Cpus
    /\ (OverdueSystems = {} \/ task \in OverdueSystems)
    /\ (ready \cap UserTasks = {} \/ systemBurst < MaxSystemBurst)
    /\ (OverdueSystems # {} \/ latencyHints = <<>> \/ latencyBurst = MaxLatencyBurst)
    /\ (OverdueSystems # {} \/ AdmittedFairPick(task, cpu, SystemTasks))
    /\ runtime' = [runtime EXCEPT ![task] = ChargeRuntime(task)]
    /\ readyAge' = AdvanceReadyAge(task)
    /\ last' = task
    /\ lastCpu' = [lastCpu EXCEPT ![task] = cpu]
    /\ systemBurst' = IF ready \cap UserTasks = {} THEN 0 ELSE systemBurst + 1
    /\ latencyBurst' = 0
    /\ fairUserPick' = TRUE
    /\ UNCHANGED <<ready, latencyHints>>

DispatchUser(task, cpu) ==
    /\ task \in ready \cap UserTasks
    /\ cpu \in Cpus
    /\ OverdueSystems = {}
    /\ AdmittedFairPick(task, cpu, UserTasks)
    /\ runtime' = [runtime EXCEPT ![task] = ChargeRuntime(task)]
    /\ readyAge' = AdvanceReadyAge(task)
    /\ last' = task
    /\ lastCpu' = [lastCpu EXCEPT ![task] = cpu]
    /\ systemBurst' = 0
    /\ latencyBurst' = 0
    /\ fairUserPick' = ObservedFairPick(task, cpu, UserTasks)
    /\ UNCHANGED <<ready, latencyHints>>

DispatchLatency(cpu) ==
    /\ latencyHints # <<>>
    /\ cpu \in Cpus
    /\ latencyBurst < MaxLatencyBurst
    /\ OverdueSystems = {}
    /\ Head(latencyHints) \in ready \cap UserTasks
    /\ LET task == Head(latencyHints) IN
       /\ runtime' = [runtime EXCEPT ![task] = ChargeRuntime(task)]
       /\ readyAge' = AdvanceReadyAge(task)
       /\ last' = task
       /\ lastCpu' = [lastCpu EXCEPT ![task] = cpu]
    /\ systemBurst' = 0
    /\ latencyBurst' = latencyBurst + 1
    /\ latencyHints' = Tail(latencyHints)
    /\ fairUserPick' = TRUE
    /\ UNCHANGED ready

QueueLatencyHint(task) ==
    /\ task \in ready \cap UserTasks
    /\ task \notin {latencyHints[index]: index \in DOMAIN latencyHints}
    /\ Len(latencyHints) < MaxLatencyHints
    /\ latencyHints' = Append(latencyHints, task)
    /\ UNCHANGED <<ready, runtime, readyAge, last, lastCpu, systemBurst,
                    latencyBurst, fairUserPick>>

DropStaleLatencyHint ==
    /\ latencyHints # <<>>
    /\ Head(latencyHints) \notin ready
    /\ latencyHints' = Tail(latencyHints)
    /\ UNCHANGED <<ready, runtime, readyAge, last, lastCpu, systemBurst,
                    latencyBurst, fairUserPick>>

Block(task) ==
    /\ task \in ready
    /\ ready' = ready \ {task}
    /\ readyAge' = [readyAge EXCEPT ![task] = 0]
    \* Keep queued hints until the consumer observes that their owner is no
    \* longer runnable.  Eagerly filtering here made DropStaleLatencyHint
    \* unreachable and failed to model the producer/consumer race.
    /\ UNCHANGED <<runtime, last, lastCpu, systemBurst, latencyBurst,
                    latencyHints, fairUserPick>>

Wake(task) ==
    /\ task \notin ready
    /\ ready' = ready \cup {task}
    /\ readyAge' = [readyAge EXCEPT ![task] = 0]
    /\ UNCHANGED <<runtime, last, lastCpu, systemBurst, latencyBurst,
                    latencyHints, fairUserPick>>

DispatchAnySystem == \E task \in SystemTasks, cpu \in Cpus: DispatchSystem(task, cpu)
DispatchAnyUser == \E task \in UserTasks, cpu \in Cpus: DispatchUser(task, cpu)
DispatchUserTask(task) == \E cpu \in Cpus: DispatchUser(task, cpu)
DispatchAnyLatency == \E cpu \in Cpus: DispatchLatency(cpu)

Next ==
    \/ DispatchAnySystem
    \/ DispatchAnyUser
    \/ DispatchAnyLatency
    \/ \E task \in UserTasks: QueueLatencyHint(task)
    \/ DropStaleLatencyHint
    \/ \E task \in Tasks: Block(task)
    \/ \E task \in Tasks: Wake(task)

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(DispatchAnySystem)
    /\ \A task \in UserTasks: SF_vars(DispatchUserTask(task))
    \* A producer can repeatedly make the queue head runnable/unrunnable.
    \* Weak fairness permits that flicker to starve every later hint forever;
    \* the consumer polling contract therefore requires strong fairness for
    \* both consuming a runnable head and dropping an observed stale head.
    /\ SF_vars(DispatchAnyLatency)
    /\ SF_vars(DropStaleLatencyHint)

TypeOK ==
    /\ ready \in SUBSET Tasks
    /\ runtime \in [Tasks -> 0..MaxRuntime]
    /\ readyAge \in [Tasks -> 0..MaxSystemWait]
    /\ last \in Tasks \cup {NoTask}
    /\ lastCpu \in [Tasks -> Cpus \cup {NoCpu}]
    /\ systemBurst \in Nat
    /\ latencyBurst \in 0..MaxLatencyBurst
    /\ latencyHints \in Seq(UserTasks)
    /\ fairUserPick \in BOOLEAN
    /\ Len(latencyHints) <= MaxLatencyHints
    /\ \A i, j \in DOMAIN latencyHints:
           i # j => latencyHints[i] # latencyHints[j]

SystemBurstIsBounded == systemBurst <= MaxSystemBurst
UserReservationIsBounded == ready \cap UserTasks # {} => systemBurst <= MaxSystemBurst
CpuAccountingIsBounded == \A task \in Tasks: runtime[task] <= MaxRuntime
OnlySystemAccumulatesReadyAge == \A task \in UserTasks: readyAge[task] = 0
FairUserSelectionUsesRuntime == fairUserPick
LatencyBurstIsBounded == latencyBurst <= MaxLatencyBurst
LatencyHintQueueIsBounded == Len(latencyHints) <= MaxLatencyHints
OverdueSystemBlocksLatency ==
    [] (OverdueSystems # {} => ~ENABLED DispatchAnyLatency)
RunnableUserEventuallyRuns ==
    \A task \in UserTasks:
        [] (task \in ready => <> (task \notin ready \/ last = task))

=============================================================================
