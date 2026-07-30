---------------------- MODULE CpuOnlineLifecycle ----------------------
EXTENDS Naturals

(***************************************************************************
Models the generation-bound CPU online state machine. No CPU may publish
scheduling authority before its private architectural state and scheduler
substrate are ready. Invalid/stale transitions are absent from Next and the
source contract turns attempts into immediate kernel panics.

Concrete owner:
  * kernel/hal/src/arch/smp.rs
***************************************************************************)

CONSTANT Cpus

Discovered == "discovered"
Starting == "starting"
OnlineParked == "online-parked"
SchedulerReady == "scheduler-ready"
Online == "online"
Quarantined == "quarantined"
Failed == "failed"

VARIABLES state, generation, privateReady, schedulerReady, dispatchAuthority

vars == <<state, generation, privateReady, schedulerReady, dispatchAuthority>>

Init ==
    /\ state = [cpu \in Cpus |-> Discovered]
    /\ generation = [cpu \in Cpus |-> 1]
    /\ privateReady = [cpu \in Cpus |-> FALSE]
    /\ schedulerReady = [cpu \in Cpus |-> FALSE]
    /\ dispatchAuthority = [cpu \in Cpus |-> FALSE]

Start(cpu) ==
    /\ state[cpu] = Discovered
    /\ state' = [state EXCEPT ![cpu] = Starting]
    /\ UNCHANGED <<generation, privateReady, schedulerReady, dispatchAuthority>>

PublishPrivateState(cpu) ==
    /\ state[cpu] = Starting
    /\ generation[cpu] = 1
    /\ state' = [state EXCEPT ![cpu] = OnlineParked]
    /\ privateReady' = [privateReady EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<generation, schedulerReady, dispatchAuthority>>

PublishScheduler(cpu) ==
    /\ state[cpu] = OnlineParked
    /\ privateReady[cpu]
    /\ state' = [state EXCEPT ![cpu] = SchedulerReady]
    /\ schedulerReady' = [schedulerReady EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<generation, privateReady, dispatchAuthority>>

AdmitDispatch(cpu) ==
    /\ state[cpu] = SchedulerReady
    /\ privateReady[cpu]
    /\ schedulerReady[cpu]
    /\ state' = [state EXCEPT ![cpu] = Online]
    /\ dispatchAuthority' = [dispatchAuthority EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<generation, privateReady, schedulerReady>>

Quarantine(cpu) ==
    /\ state[cpu] = Online
    /\ state' = [state EXCEPT ![cpu] = Quarantined]
    /\ dispatchAuthority' = [dispatchAuthority EXCEPT ![cpu] = FALSE]
    /\ UNCHANGED <<generation, privateReady, schedulerReady>>

Fail(cpu) ==
    /\ state[cpu] \in {Starting, OnlineParked, SchedulerReady, Quarantined}
    /\ state' = [state EXCEPT ![cpu] = Failed]
    /\ dispatchAuthority' = [dispatchAuthority EXCEPT ![cpu] = FALSE]
    /\ UNCHANGED <<generation, privateReady, schedulerReady>>

Terminal ==
    /\ \A cpu \in Cpus : state[cpu] = Failed
    /\ UNCHANGED vars

Next ==
    \/ \E cpu \in Cpus:
        Start(cpu)
        \/ PublishPrivateState(cpu)
        \/ PublishScheduler(cpu)
        \/ AdmitDispatch(cpu)
        \/ Quarantine(cpu)
        \/ Fail(cpu)
    \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ state \in [Cpus ->
        {Discovered, Starting, OnlineParked, SchedulerReady, Online,
         Quarantined, Failed}]
    /\ generation \in [Cpus -> Nat]
    /\ privateReady \in [Cpus -> BOOLEAN]
    /\ schedulerReady \in [Cpus -> BOOLEAN]
    /\ dispatchAuthority \in [Cpus -> BOOLEAN]

GenerationNeverAliases ==
    \A cpu \in Cpus : generation[cpu] = 1

DispatchRequiresCompleteCpu ==
    \A cpu \in Cpus:
        dispatchAuthority[cpu] =>
            /\ state[cpu] = Online
            /\ privateReady[cpu]
            /\ schedulerReady[cpu]

OnlineRequiresCompleteCpu ==
    \A cpu \in Cpus:
        state[cpu] = Online =>
            /\ privateReady[cpu]
            /\ schedulerReady[cpu]
            /\ dispatchAuthority[cpu]

FailedCpuOwnsNoDispatch ==
    \A cpu \in Cpus : state[cpu] = Failed => ~dispatchAuthority[cpu]

=============================================================================
