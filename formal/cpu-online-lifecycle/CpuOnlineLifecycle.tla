---------------------- MODULE CpuOnlineLifecycle ----------------------
EXTENDS Naturals

(***************************************************************************
Models the boot-static generation-bound CPU online state machine. No CPU may
publish scheduling authority before its private architectural state and
scheduler substrate are ready. Hot-unplug and in-boot restart are deliberately
unsupported: Failed is terminal until a machine reboot creates a fresh model
instance. Stale-generation and restart attempts are explicit fail-stop actions,
not silently absent behaviors.

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

VARIABLES state, generation, privateReady, schedulerReady, dispatchAuthority,
          failedSeen, staleGenerationAttempted, restartAttempted, panicRaised

vars == <<state, generation, privateReady, schedulerReady, dispatchAuthority,
          failedSeen, staleGenerationAttempted, restartAttempted, panicRaised>>

Init ==
    /\ state = [cpu \in Cpus |-> Discovered]
    /\ generation = [cpu \in Cpus |-> 1]
    /\ privateReady = [cpu \in Cpus |-> FALSE]
    /\ schedulerReady = [cpu \in Cpus |-> FALSE]
    /\ dispatchAuthority = [cpu \in Cpus |-> FALSE]
    /\ failedSeen = [cpu \in Cpus |-> FALSE]
    /\ staleGenerationAttempted = FALSE
    /\ restartAttempted = FALSE
    /\ panicRaised = FALSE

Start(cpu) ==
    /\ state[cpu] = Discovered
    /\ state' = [state EXCEPT ![cpu] = Starting]
    /\ UNCHANGED <<generation, privateReady, schedulerReady, dispatchAuthority,
                   failedSeen, staleGenerationAttempted, restartAttempted,
                   panicRaised>>

PublishPrivateState(cpu) ==
    /\ state[cpu] = Starting
    /\ generation[cpu] = 1
    /\ state' = [state EXCEPT ![cpu] = OnlineParked]
    /\ privateReady' = [privateReady EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<generation, schedulerReady, dispatchAuthority, failedSeen,
                   staleGenerationAttempted, restartAttempted, panicRaised>>

PublishScheduler(cpu) ==
    /\ state[cpu] = OnlineParked
    /\ privateReady[cpu]
    /\ state' = [state EXCEPT ![cpu] = SchedulerReady]
    /\ schedulerReady' = [schedulerReady EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<generation, privateReady, dispatchAuthority, failedSeen,
                   staleGenerationAttempted, restartAttempted, panicRaised>>

AdmitDispatch(cpu) ==
    /\ state[cpu] = SchedulerReady
    /\ privateReady[cpu]
    /\ schedulerReady[cpu]
    /\ state' = [state EXCEPT ![cpu] = Online]
    /\ dispatchAuthority' = [dispatchAuthority EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<generation, privateReady, schedulerReady, failedSeen,
                   staleGenerationAttempted, restartAttempted, panicRaised>>

Quarantine(cpu) ==
    /\ state[cpu] = Online
    /\ state' = [state EXCEPT ![cpu] = Quarantined]
    /\ dispatchAuthority' = [dispatchAuthority EXCEPT ![cpu] = FALSE]
    /\ UNCHANGED <<generation, privateReady, schedulerReady, failedSeen,
                   staleGenerationAttempted, restartAttempted, panicRaised>>

Fail(cpu) ==
    /\ state[cpu] \in {Starting, OnlineParked, SchedulerReady, Quarantined}
    /\ state' = [state EXCEPT ![cpu] = Failed]
    /\ dispatchAuthority' = [dispatchAuthority EXCEPT ![cpu] = FALSE]
    /\ failedSeen' = [failedSeen EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<generation, privateReady, schedulerReady,
                   staleGenerationAttempted, restartAttempted, panicRaised>>

RejectStaleGeneration ==
    /\ staleGenerationAttempted' = TRUE
    /\ panicRaised' = TRUE
    /\ UNCHANGED <<state, generation, privateReady, schedulerReady,
                   dispatchAuthority, failedSeen, restartAttempted>>

RejectRestart(cpu) ==
    /\ state[cpu] = Failed
    /\ restartAttempted' = TRUE
    /\ panicRaised' = TRUE
    /\ UNCHANGED <<state, generation, privateReady, schedulerReady,
                   dispatchAuthority, failedSeen, staleGenerationAttempted>>

Terminal ==
    /\ \A cpu \in Cpus : state[cpu] = Failed
    /\ UNCHANGED vars

PanicTerminal ==
    /\ panicRaised
    /\ UNCHANGED vars

Next ==
    IF panicRaised
    THEN PanicTerminal
    ELSE
        \/ \E cpu \in Cpus:
            Start(cpu)
            \/ PublishPrivateState(cpu)
            \/ PublishScheduler(cpu)
            \/ AdmitDispatch(cpu)
            \/ Quarantine(cpu)
            \/ Fail(cpu)
            \/ RejectRestart(cpu)
        \/ RejectStaleGeneration
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
    /\ failedSeen \in [Cpus -> BOOLEAN]
    /\ staleGenerationAttempted \in BOOLEAN
    /\ restartAttempted \in BOOLEAN
    /\ panicRaised \in BOOLEAN

BootEpochGenerationIsFixed ==
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

FailedStateIsTerminalForBoot ==
    \A cpu \in Cpus : failedSeen[cpu] => state[cpu] = Failed

InvalidGenerationFailsClosed == staleGenerationAttempted => panicRaised

RestartFailsClosed == restartAttempted => panicRaised

=============================================================================
