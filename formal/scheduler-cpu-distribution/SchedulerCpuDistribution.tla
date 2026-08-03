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
    /\ systemBurst = [cpu \in Cpus |-> 0]
    /\ latencyBurst = [cpu \in Cpus |-> 0]
    /\ latencyHints = [cpu \in Cpus |-> <<>>]
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
    /\ (ready \cap UserTasks = {} \/ systemBurst[cpu] < MaxSystemBurst)
    /\ (OverdueSystems # {} \/ latencyHints[cpu] = <<>> \/ latencyBurst[cpu] = MaxLatencyBurst)
    /\ (OverdueSystems # {} \/ AdmittedFairPick(task, cpu, SystemTasks))
    /\ runtime' = [runtime EXCEPT ![task] = ChargeRuntime(task)]
    /\ readyAge' = AdvanceReadyAge(task)
    /\ last' = task
    /\ lastCpu' = [lastCpu EXCEPT ![task] = cpu]
    /\ systemBurst' =
        [systemBurst EXCEPT
            ![cpu] = IF ready \cap UserTasks = {} THEN 0 ELSE @ + 1]
    /\ latencyBurst' = [latencyBurst EXCEPT ![cpu] = 0]
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
    /\ systemBurst' = [systemBurst EXCEPT ![cpu] = 0]
    /\ latencyBurst' = [latencyBurst EXCEPT ![cpu] = 0]
    /\ fairUserPick' = ObservedFairPick(task, cpu, UserTasks)
    /\ UNCHANGED <<ready, latencyHints>>

DispatchLatency(cpu) ==
    /\ latencyHints[cpu] # <<>>
    /\ cpu \in Cpus
    /\ latencyBurst[cpu] < MaxLatencyBurst
    /\ OverdueSystems = {}
    /\ Head(latencyHints[cpu]) \in ready \cap UserTasks
    /\ LET task == Head(latencyHints[cpu]) IN
       /\ runtime' = [runtime EXCEPT ![task] = ChargeRuntime(task)]
       /\ readyAge' = AdvanceReadyAge(task)
       /\ last' = task
       /\ lastCpu' = [lastCpu EXCEPT ![task] = cpu]
    /\ systemBurst' = [systemBurst EXCEPT ![cpu] = 0]
    /\ latencyBurst' = [latencyBurst EXCEPT ![cpu] = @ + 1]
    /\ latencyHints' = [latencyHints EXCEPT ![cpu] = Tail(@)]
    /\ fairUserPick' = TRUE
    /\ UNCHANGED ready

QueueLatencyHint(task, cpu) ==
    /\ task \in ready \cap UserTasks
    /\ cpu \in Cpus
    /\ task \notin {latencyHints[cpu][index]: index \in DOMAIN latencyHints[cpu]}
    /\ Len(latencyHints[cpu]) < MaxLatencyHints
    /\ latencyHints' = [latencyHints EXCEPT ![cpu] = Append(@, task)]
    /\ UNCHANGED <<ready, runtime, readyAge, last, lastCpu, systemBurst,
                    latencyBurst, fairUserPick>>

DropStaleLatencyHint(cpu) ==
    /\ cpu \in Cpus
    /\ latencyHints[cpu] # <<>>
    /\ Head(latencyHints[cpu]) \notin ready
    /\ latencyHints' = [latencyHints EXCEPT ![cpu] = Tail(@)]
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
    \/ \E task \in UserTasks, cpu \in Cpus: QueueLatencyHint(task, cpu)
    \/ \E cpu \in Cpus: DropStaleLatencyHint(cpu)
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
    /\ (\A cpu \in Cpus: SF_vars(DropStaleLatencyHint(cpu)))

TypeOK ==
    /\ ready \in SUBSET Tasks
    /\ runtime \in [Tasks -> 0..MaxRuntime]
    /\ readyAge \in [Tasks -> 0..MaxSystemWait]
    /\ last \in Tasks \cup {NoTask}
    /\ lastCpu \in [Tasks -> Cpus \cup {NoCpu}]
    /\ systemBurst \in [Cpus -> Nat]
    /\ latencyBurst \in [Cpus -> 0..MaxLatencyBurst]
    /\ latencyHints \in [Cpus -> Seq(UserTasks)]
    /\ fairUserPick \in BOOLEAN
    /\ \A cpu \in Cpus:
        /\ Len(latencyHints[cpu]) <= MaxLatencyHints
        /\ \A i, j \in DOMAIN latencyHints[cpu]:
               i # j => latencyHints[cpu][i] # latencyHints[cpu][j]

SystemBurstIsBounded == \A cpu \in Cpus: systemBurst[cpu] <= MaxSystemBurst
UserReservationIsBounded ==
    ready \cap UserTasks # {} => \A cpu \in Cpus: systemBurst[cpu] <= MaxSystemBurst
CpuAccountingIsBounded == \A task \in Tasks: runtime[task] <= MaxRuntime
OnlySystemAccumulatesReadyAge == \A task \in UserTasks: readyAge[task] = 0
FairUserSelectionUsesRuntime == fairUserPick
LatencyBurstIsBounded == \A cpu \in Cpus: latencyBurst[cpu] <= MaxLatencyBurst
LatencyHintQueueIsBounded ==
    \A cpu \in Cpus: Len(latencyHints[cpu]) <= MaxLatencyHints
OverdueSystemBlocksLatency ==
    [] (OverdueSystems # {} => ~ENABLED DispatchAnyLatency)
RunnableUserEventuallyRuns ==
    \A task \in UserTasks:
        [] (task \in ready => <> (task \notin ready \/ last = task))

=============================================================================
