-------------------- MODULE PerCpuClockeventLifecycle --------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Models per-CPU clockevent admission and interrupt ordering. An AP cannot become
Online until its private timer is programmed, and a delivered deadline is
rearmed into the future before scheduler work and local EOI complete.
***************************************************************************)

CONSTANT Cpus, ApCpus, MaxTime

Offline == "offline"
Programmed == "programmed"
Online == "online"
Pending == "pending"
Rearmed == "rearmed"

VARIABLES now, state, deadline, schedulerTurns

vars == <<now, state, deadline, schedulerTurns>>

Init ==
    /\ now = 0
    /\ state = [cpu \in Cpus |-> IF cpu \in ApCpus THEN Offline ELSE Online]
    /\ deadline = [cpu \in Cpus |-> IF cpu \in ApCpus THEN 0 ELSE 1]
    /\ schedulerTurns = [cpu \in Cpus |-> 0]

ProgramAp(cpu) ==
    /\ cpu \in ApCpus
    /\ state[cpu] = Offline
    /\ state' = [state EXCEPT ![cpu] = Programmed]
    /\ deadline' = [deadline EXCEPT ![cpu] = now + 1]
    /\ UNCHANGED <<now, schedulerTurns>>

PublishOnline(cpu) ==
    /\ cpu \in ApCpus
    /\ state[cpu] = Programmed
    /\ deadline[cpu] > now
    /\ state' = [state EXCEPT ![cpu] = Online]
    /\ UNCHANGED <<now, deadline, schedulerTurns>>

Advance ==
    /\ now < MaxTime
    /\ \A cpu \in Cpus: state[cpu] \notin {Programmed, Rearmed}
    /\ now' = now + 1
    /\ UNCHANGED <<state, deadline, schedulerTurns>>

Deliver(cpu) ==
    /\ state[cpu] = Online
    /\ deadline[cpu] <= now
    /\ state' = [state EXCEPT ![cpu] = Pending]
    /\ UNCHANGED <<now, deadline, schedulerTurns>>

Rearm(cpu) ==
    /\ state[cpu] = Pending
    /\ state' = [state EXCEPT ![cpu] = Rearmed]
    /\ deadline' = [deadline EXCEPT ![cpu] = now + 1]
    /\ UNCHANGED <<now, schedulerTurns>>

ScheduleAndEoi(cpu) ==
    /\ state[cpu] = Rearmed
    /\ deadline[cpu] > now
    /\ state' = [state EXCEPT ![cpu] = Online]
    /\ schedulerTurns' =
        [schedulerTurns EXCEPT ![cpu] = schedulerTurns[cpu] + 1]
    /\ UNCHANGED <<now, deadline>>

Terminal ==
    /\ now = MaxTime
    /\ \A cpu \in Cpus: state[cpu] \in {Offline, Online}
    /\ UNCHANGED vars

Next ==
    \/ \E cpu \in Cpus: ProgramAp(cpu)
    \/ \E cpu \in Cpus: PublishOnline(cpu)
    \/ Advance
    \/ \E cpu \in Cpus: Deliver(cpu)
    \/ \E cpu \in Cpus: Rearm(cpu)
    \/ \E cpu \in Cpus: ScheduleAndEoi(cpu)
    \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ now \in 0..MaxTime
    /\ state \in [Cpus -> {Offline, Programmed, Online, Pending, Rearmed}]
    /\ deadline \in [Cpus -> Nat]
    /\ schedulerTurns \in [Cpus -> Nat]

ApOnlineRequiresProgrammedDeadline ==
    \A cpu \in ApCpus:
        state[cpu] \in {Online, Pending, Rearmed} => deadline[cpu] > 0

RearmedDeadlineIsFuture ==
    \A cpu \in Cpus: state[cpu] = Rearmed => deadline[cpu] > now

SchedulerTurnRequiresRearm ==
    \A cpu \in Cpus: schedulerTurns[cpu] > 0 => deadline[cpu] > 0

=============================================================================
