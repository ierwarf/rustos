------------------------------- MODULE DeferredStart -------------------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Models loaderd deferred-start launches. A normal supervised child is created
suspended, admitted by its designated supervisor for the exact PID, activated
once, and only then allowed to register an endpoint. The model treats an
endpoint wait timeout or pre-activation exit as terminal for that launch.
***************************************************************************)

CONSTANTS Services, Supervisors, Pids, WaitTimeout, MaxTime

NoPid == 0
NoSupervisor == "none"
InitdSupervisor == "initd"
RuntimedSupervisor == "runtimed"
Absent == "absent"
Suspended == "suspended"
Running == "running"
Exited == "exited"

Idle == "idle"
Waiting == "waiting"
Ready == "ready"
TimedOut == "timed-out"
Failed == "failed"

SupervisorFor(s) ==
    IF s = "netd" THEN InitdSupervisor ELSE RuntimedSupervisor

VARIABLES procState,
          procPid,
          leasePid,
          spawnSupervisor,
          admittedSupervisor,
          endpointPid,
          activationCount,
          waitState,
          waitPid,
          waitDeadline,
          issuedPids,
          supervisorHealthy,
          uncertainPid,
          now

vars == <<procState, procPid, leasePid, spawnSupervisor, admittedSupervisor,
          endpointPid, activationCount, waitState, waitPid, waitDeadline,
          issuedPids, supervisorHealthy, uncertainPid, now>>

Init ==
    /\ procState = [s \in Services |-> Absent]
    /\ procPid = [s \in Services |-> NoPid]
    /\ leasePid = [s \in Services |-> NoPid]
    /\ spawnSupervisor = [s \in Services |-> NoSupervisor]
    /\ admittedSupervisor = [s \in Services |-> NoSupervisor]
    /\ endpointPid = [s \in Services |-> NoPid]
    /\ activationCount = [p \in Pids |-> 0]
    /\ waitState = [s \in Services |-> Idle]
    /\ waitPid = [s \in Services |-> NoPid]
    /\ waitDeadline = [s \in Services |-> 0]
    /\ issuedPids = {}
    /\ supervisorHealthy = TRUE
    /\ uncertainPid = NoPid
    /\ now = 0

SpawnDeferred(s, p, supervisor) ==
    /\ supervisorHealthy
    /\ s \in Services
    /\ p \in Pids \ issuedPids
    /\ supervisor = SupervisorFor(s)
    /\ procState[s] \in {Absent, Exited}
    /\ procState' = [procState EXCEPT ![s] = Suspended]
    /\ procPid' = [procPid EXCEPT ![s] = p]
    /\ spawnSupervisor' = [spawnSupervisor EXCEPT ![s] = supervisor]
    /\ admittedSupervisor' = [admittedSupervisor EXCEPT ![s] = NoSupervisor]
    /\ waitState' = [waitState EXCEPT ![s] = Idle]
    /\ waitPid' = [waitPid EXCEPT ![s] = NoPid]
    /\ waitDeadline' = [waitDeadline EXCEPT ![s] = 0]
    /\ issuedPids' = issuedPids \cup {p}
    /\ UNCHANGED <<leasePid, endpointPid, activationCount, supervisorHealthy, uncertainPid, now>>

AdmitLease(s, supervisor) ==
    /\ supervisorHealthy
    /\ s \in Services
    /\ supervisor = SupervisorFor(s)
    /\ procState[s] = Suspended
    /\ procPid[s] # NoPid
    /\ leasePid[s] = NoPid
    /\ spawnSupervisor[s] = supervisor
    /\ leasePid' = [leasePid EXCEPT ![s] = procPid[s]]
    /\ admittedSupervisor' = [admittedSupervisor EXCEPT ![s] = supervisor]
    /\ UNCHANGED <<procState, procPid, spawnSupervisor, endpointPid,
                  activationCount, waitState, waitPid, waitDeadline,
                  issuedPids, supervisorHealthy, uncertainPid, now>>

Activate(s, supervisor) ==
    /\ supervisorHealthy
    /\ s \in Services
    /\ supervisor = SupervisorFor(s)
    /\ procState[s] = Suspended
    /\ procPid[s] # NoPid
    /\ leasePid[s] = procPid[s]
    /\ spawnSupervisor[s] = supervisor
    /\ admittedSupervisor[s] = supervisor
    /\ activationCount[procPid[s]] = 0
    \* Keep every finite-model wait inside the explored clock horizon.
    /\ now <= MaxTime - WaitTimeout
    /\ procState' = [procState EXCEPT ![s] = Running]
    /\ activationCount' = [activationCount EXCEPT ![procPid[s]] = @ + 1]
    /\ waitState' = [waitState EXCEPT ![s] = Waiting]
    /\ waitPid' = [waitPid EXCEPT ![s] = procPid[s]]
    /\ waitDeadline' = [waitDeadline EXCEPT ![s] = now + WaitTimeout]
    /\ UNCHANGED <<procPid, leasePid, spawnSupervisor, admittedSupervisor,
                  endpointPid, issuedPids, supervisorHealthy, uncertainPid, now>>

RegisterEndpoint(s) ==
    /\ supervisorHealthy
    /\ s \in Services
    /\ procState[s] = Running
    /\ procPid[s] = leasePid[s]
    /\ activationCount[procPid[s]] = 1
    /\ endpointPid[s] = NoPid
    /\ waitState[s] = Waiting
    /\ now < waitDeadline[s]
    /\ endpointPid' = [endpointPid EXCEPT ![s] = procPid[s]]
    /\ waitState' = [waitState EXCEPT ![s] = Ready]
    /\ UNCHANGED <<procState, procPid, leasePid, spawnSupervisor,
                  admittedSupervisor, activationCount, waitPid, waitDeadline,
                  issuedPids, supervisorHealthy, uncertainPid, now>>

Exit(s) ==
    /\ supervisorHealthy
    /\ s \in Services
    /\ procState[s] \in {Suspended, Running}
    /\ procState' = [procState EXCEPT ![s] = Exited]
    /\ procPid' = [procPid EXCEPT ![s] = NoPid]
    /\ leasePid' = [leasePid EXCEPT ![s] = NoPid]
    /\ spawnSupervisor' = [spawnSupervisor EXCEPT ![s] = NoSupervisor]
    /\ admittedSupervisor' = [admittedSupervisor EXCEPT ![s] = NoSupervisor]
    /\ endpointPid' = [endpointPid EXCEPT ![s] = NoPid]
    /\ waitState' = [waitState EXCEPT ![s] = Failed]
    /\ waitPid' = [waitPid EXCEPT ![s] = NoPid]
    /\ waitDeadline' = [waitDeadline EXCEPT ![s] = 0]
    /\ UNCHANGED <<activationCount, issuedPids, supervisorHealthy, uncertainPid, now>>

ActivationFailsAndCleans(s, supervisor) ==
    /\ supervisorHealthy
    /\ s \in Services
    /\ supervisor = SupervisorFor(s)
    /\ procState[s] = Suspended
    /\ leasePid[s] = procPid[s]
    /\ admittedSupervisor[s] = supervisor
    /\ procState' = [procState EXCEPT ![s] = Exited]
    /\ procPid' = [procPid EXCEPT ![s] = NoPid]
    /\ leasePid' = [leasePid EXCEPT ![s] = NoPid]
    /\ spawnSupervisor' = [spawnSupervisor EXCEPT ![s] = NoSupervisor]
    /\ admittedSupervisor' = [admittedSupervisor EXCEPT ![s] = NoSupervisor]
    /\ waitState' = [waitState EXCEPT ![s] = Failed]
    /\ UNCHANGED <<endpointPid, activationCount, waitPid, waitDeadline, issuedPids,
                  supervisorHealthy, uncertainPid, now>>

ActivationCleanupFails(s, supervisor) ==
    /\ supervisorHealthy
    /\ s \in Services
    /\ supervisor = SupervisorFor(s)
    /\ procState[s] = Suspended
    /\ leasePid[s] = procPid[s]
    /\ admittedSupervisor[s] = supervisor
    /\ supervisorHealthy' = FALSE
    /\ uncertainPid' = procPid[s]
    /\ UNCHANGED <<procState, procPid, leasePid, spawnSupervisor, admittedSupervisor,
                  endpointPid, activationCount, waitState, waitPid, waitDeadline,
                  issuedPids, now>>

AdvanceTime ==
    /\ supervisorHealthy
    /\ now < MaxTime
    /\ now' = now + 1
    /\ procState' =
         [s \in Services |->
            IF waitState[s] = Waiting /\ now + 1 >= waitDeadline[s]
            THEN Exited
            ELSE procState[s]]
    /\ procPid' =
         [s \in Services |->
            IF waitState[s] = Waiting /\ now + 1 >= waitDeadline[s]
            THEN NoPid
            ELSE procPid[s]]
    /\ leasePid' =
         [s \in Services |->
            IF waitState[s] = Waiting /\ now + 1 >= waitDeadline[s]
            THEN NoPid
            ELSE leasePid[s]]
    /\ spawnSupervisor' =
         [s \in Services |->
            IF waitState[s] = Waiting /\ now + 1 >= waitDeadline[s]
            THEN NoSupervisor
            ELSE spawnSupervisor[s]]
    /\ admittedSupervisor' =
         [s \in Services |->
            IF waitState[s] = Waiting /\ now + 1 >= waitDeadline[s]
            THEN NoSupervisor
            ELSE admittedSupervisor[s]]
    /\ endpointPid' =
         [s \in Services |->
            IF waitState[s] = Waiting /\ now + 1 >= waitDeadline[s]
            THEN NoPid
            ELSE endpointPid[s]]
    /\ waitState' =
         [s \in Services |->
            IF waitState[s] = Waiting /\ now + 1 >= waitDeadline[s]
            THEN TimedOut
            ELSE waitState[s]]
    /\ waitPid' =
         [s \in Services |->
            IF waitState[s] = Waiting /\ now + 1 >= waitDeadline[s]
            THEN NoPid
            ELSE waitPid[s]]
    /\ UNCHANGED <<activationCount, waitDeadline, issuedPids, supervisorHealthy, uncertainPid>>

TimeoutCleanupFails(s) ==
    /\ supervisorHealthy
    /\ s \in Services
    /\ now < MaxTime
    /\ waitState[s] = Waiting
    /\ now + 1 >= waitDeadline[s]
    /\ now' = now + 1
    /\ supervisorHealthy' = FALSE
    /\ uncertainPid' = procPid[s]
    /\ waitState' = [waitState EXCEPT ![s] = TimedOut]
    /\ UNCHANGED <<procState, procPid, leasePid, spawnSupervisor, admittedSupervisor,
                  endpointPid, activationCount, waitPid, waitDeadline, issuedPids>>

Next ==
    \/ \E s \in Services, p \in Pids, supervisor \in Supervisors :
        SpawnDeferred(s, p, supervisor)
    \/ \E s \in Services, supervisor \in Supervisors : AdmitLease(s, supervisor)
    \/ \E s \in Services, supervisor \in Supervisors : Activate(s, supervisor)
    \/ \E s \in Services, supervisor \in Supervisors :
        ActivationFailsAndCleans(s, supervisor) \/ ActivationCleanupFails(s, supervisor)
    \/ \E s \in Services : RegisterEndpoint(s)
    \/ \E s \in Services : Exit(s)
    \/ AdvanceTime
    \/ \E s \in Services : TimeoutCleanupFails(s)

TypeOK ==
    /\ Services \subseteq STRING
    /\ Supervisors \subseteq STRING
    /\ Services = {"netd", "uiserver"}
    /\ Supervisors = {InitdSupervisor, RuntimedSupervisor}
    /\ Pids \subseteq Nat
    /\ NoPid \notin Pids
    /\ WaitTimeout \in Nat \ {0}
    /\ MaxTime \in Nat
    /\ procState \in [Services -> {Absent, Suspended, Running, Exited}]
    /\ procPid \in [Services -> (Pids \cup {NoPid})]
    /\ leasePid \in [Services -> (Pids \cup {NoPid})]
    /\ spawnSupervisor \in [Services -> (Supervisors \cup {NoSupervisor})]
    /\ admittedSupervisor \in [Services -> (Supervisors \cup {NoSupervisor})]
    /\ endpointPid \in [Services -> (Pids \cup {NoPid})]
    /\ activationCount \in [Pids -> 0..1]
    /\ waitState \in [Services -> {Idle, Waiting, Ready, TimedOut, Failed}]
    /\ waitPid \in [Services -> (Pids \cup {NoPid})]
    /\ waitDeadline \in [Services -> 0..MaxTime]
    /\ issuedPids \subseteq Pids
    /\ supervisorHealthy \in BOOLEAN
    /\ uncertainPid \in Pids \cup {NoPid}
    /\ now \in 0..MaxTime

SuspendedChildrenAreInert ==
    \A s \in Services:
        procState[s] = Suspended =>
            /\ procPid[s] # NoPid
            /\ leasePid[s] \in {NoPid, procPid[s]}
            /\ endpointPid[s] = NoPid
            /\ waitState[s] = Idle
            /\ waitPid[s] = NoPid
            /\ waitDeadline[s] = 0

RunningChildrenHaveExactAdmission ==
    \A s \in Services:
        procState[s] = Running =>
            /\ procPid[s] # NoPid
            /\ leasePid[s] = procPid[s]
            /\ spawnSupervisor[s] = SupervisorFor(s)
            /\ admittedSupervisor[s] = SupervisorFor(s)
            /\ activationCount[procPid[s]] = 1
            /\ waitPid[s] = procPid[s]
            /\ waitState[s] \in {Waiting, Ready, TimedOut}

EndpointRequiresSingleActivation ==
    \A s \in Services:
        endpointPid[s] # NoPid =>
            /\ procState[s] = Running
            /\ endpointPid[s] = procPid[s]
            /\ endpointPid[s] = leasePid[s]
            /\ activationCount[endpointPid[s]] = 1
            /\ waitState[s] = Ready

ActivationIsSingleUse ==
    \A p \in Pids:
        activationCount[p] \in {0, 1}

ExitedChildrenLeaveNoBindings ==
    \A s \in Services:
        procState[s] \in {Absent, Exited} =>
            /\ procPid[s] = NoPid
            /\ leasePid[s] = NoPid
            /\ endpointPid[s] = NoPid
            /\ spawnSupervisor[s] = NoSupervisor
            /\ admittedSupervisor[s] = NoSupervisor
            /\ waitPid[s] = NoPid
            /\ IF procState[s] = Absent
                  THEN waitState[s] = Idle /\ waitDeadline[s] = 0
                  ELSE
                    /\ waitState[s] \in {Failed, TimedOut}
                    /\ waitState[s] = Failed => waitDeadline[s] = 0
                    /\ waitState[s] = TimedOut => 0 < waitDeadline[s] /\ waitDeadline[s] <= now

WaitOutcomeIsBounded ==
    \A s \in Services:
        /\ waitState[s] = Waiting => now < waitDeadline[s] \/ ~supervisorHealthy
        /\ waitState[s] = TimedOut => now >= waitDeadline[s]
        /\ waitState[s] = Ready =>
            /\ endpointPid[s] = waitPid[s]
            /\ endpointPid[s] # NoPid

WaitEventuallySettles ==
    \A s \in Services:
        waitState[s] = Waiting ~> waitState[s] # Waiting \/ ~supervisorHealthy

HealthySupervisorHasNoUncertainChild ==
    supervisorHealthy <=> uncertainPid = NoPid

UncertainCleanupCannotHavePublishedEndpoint ==
    uncertainPid # NoPid =>
        \A s \in Services: procPid[s] = uncertainPid => endpointPid[s] = NoPid

AllLivePidsWereIssued ==
    \A s \in Services:
        /\ procPid[s] # NoPid => procPid[s] \in issuedPids
        /\ leasePid[s] # NoPid => leasePid[s] \in issuedPids
        /\ endpointPid[s] # NoPid => endpointPid[s] \in issuedPids
        /\ waitPid[s] # NoPid => waitPid[s] \in issuedPids

DistinctLivePids ==
    \A s \in Services:
        \A t \in Services:
            /\ s # t
            /\ procPid[s] # NoPid
            /\ procPid[t] # NoPid
            => procPid[s] # procPid[t]

Spec == Init /\ [][Next]_vars /\ WF_vars(AdvanceTime)

=============================================================================
