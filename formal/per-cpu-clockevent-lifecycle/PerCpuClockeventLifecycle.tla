-------------------- MODULE PerCpuClockeventLifecycle --------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Models per-CPU clockevent admission and interrupt ordering. An AP cannot become
Online until its private timer is programmed, and a delivered deadline is
rearmed into the future before scheduler work and local EOI complete.
***************************************************************************)

CONSTANT Cpus, ApCpus, MaxTime

Offline == "offline"
LvtReady == "lvt-ready"
Armed == "armed"
Online == "online"
Pending == "pending"
Serviced == "serviced"
Rearmed == "rearmed"

VARIABLES now, state, deadline, lvtUnmasked, schedulerTurns

vars == <<now, state, deadline, lvtUnmasked, schedulerTurns>>

Init ==
    /\ now = 0
    /\ state = [cpu \in Cpus |-> IF cpu \in ApCpus THEN Offline ELSE Online]
    /\ deadline = [cpu \in Cpus |-> IF cpu \in ApCpus THEN 0 ELSE 1]
    /\ lvtUnmasked = [cpu \in Cpus |-> cpu \notin ApCpus]
    /\ schedulerTurns = [cpu \in Cpus |-> 0]

ConfigureLvt(cpu) ==
    /\ cpu \in ApCpus
    /\ state[cpu] = Offline
    /\ state' = [state EXCEPT ![cpu] = LvtReady]
    /\ lvtUnmasked' = [lvtUnmasked EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<now, deadline, schedulerTurns>>

ProgramDeadline(cpu) ==
    /\ cpu \in ApCpus
    /\ state[cpu] = LvtReady
    /\ lvtUnmasked[cpu]
    /\ state' = [state EXCEPT ![cpu] = Armed]
    /\ deadline' = [deadline EXCEPT ![cpu] = now + 1]
    /\ UNCHANGED <<now, lvtUnmasked, schedulerTurns>>

PublishOnline(cpu) ==
    /\ cpu \in ApCpus
    /\ state[cpu] = Armed
    /\ deadline[cpu] > now
    /\ state' = [state EXCEPT ![cpu] = Online]
    /\ UNCHANGED <<now, deadline, lvtUnmasked, schedulerTurns>>

Advance ==
    /\ now < MaxTime
    /\ \A cpu \in Cpus: state[cpu] \notin {Armed, Rearmed}
    /\ now' = now + 1
    /\ UNCHANGED <<state, deadline, lvtUnmasked, schedulerTurns>>

Deliver(cpu) ==
    /\ state[cpu] = Online
    /\ deadline[cpu] <= now
    /\ state' = [state EXCEPT ![cpu] = Pending]
    /\ UNCHANGED <<now, deadline, lvtUnmasked, schedulerTurns>>

Rearm(cpu) ==
    /\ state[cpu] = Serviced
    /\ state' = [state EXCEPT ![cpu] = Rearmed]
    /\ deadline' = [deadline EXCEPT ![cpu] = now + 1]
    /\ UNCHANGED <<now, lvtUnmasked, schedulerTurns>>

ServiceClockevent(cpu) ==
    /\ state[cpu] = Pending
    /\ state' = [state EXCEPT ![cpu] = Serviced]
    /\ schedulerTurns' =
        [schedulerTurns EXCEPT ![cpu] = schedulerTurns[cpu] + 1]
    /\ UNCHANGED <<now, deadline, lvtUnmasked>>

CompleteEoi(cpu) ==
    /\ state[cpu] = Rearmed
    /\ deadline[cpu] > now
    /\ state' = [state EXCEPT ![cpu] = Online]
    /\ UNCHANGED <<now, deadline, lvtUnmasked, schedulerTurns>>

Terminal ==
    /\ now = MaxTime
    /\ \A cpu \in Cpus: state[cpu] \in {Offline, Online}
    /\ UNCHANGED vars

Next ==
    \/ \E cpu \in Cpus: ConfigureLvt(cpu)
    \/ \E cpu \in Cpus: ProgramDeadline(cpu)
    \/ \E cpu \in Cpus: PublishOnline(cpu)
    \/ Advance
    \/ \E cpu \in Cpus: Deliver(cpu)
    \/ \E cpu \in Cpus: ServiceClockevent(cpu)
    \/ \E cpu \in Cpus: Rearm(cpu)
    \/ \E cpu \in Cpus: CompleteEoi(cpu)
    \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ now \in 0..MaxTime
    /\ state \in [Cpus -> {Offline, LvtReady, Armed, Online, Pending, Serviced, Rearmed}]
    /\ deadline \in [Cpus -> Nat]
    /\ lvtUnmasked \in [Cpus -> BOOLEAN]
    /\ schedulerTurns \in [Cpus -> Nat]

ApOnlineRequiresProgrammedDeadline ==
    \A cpu \in ApCpus:
        state[cpu] \in {Armed, Online, Pending, Serviced, Rearmed} => deadline[cpu] > 0

ArmedDeadlineRequiresUnmaskedLvt ==
    \A cpu \in ApCpus:
        state[cpu] \in {Armed, Online, Pending, Serviced, Rearmed} => lvtUnmasked[cpu]

RearmedDeadlineIsFuture ==
    \A cpu \in Cpus: state[cpu] = Rearmed => deadline[cpu] > now

SchedulerTurnRequiresRearm ==
    \A cpu \in Cpus: schedulerTurns[cpu] > 0 => deadline[cpu] > 0

=============================================================================
