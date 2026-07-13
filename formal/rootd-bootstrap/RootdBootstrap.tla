------------------------------ MODULE RootdBootstrap ------------------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
This abstracts the supervised service launch transaction. It does not model
ELF or PE loading, IPC payload bytes, or the scheduler implementation.

PIDs are finite and never reused. That makes stale endpoint or capability
references observable instead of letting later PID reuse hide them.
AdvanceTime represents the deadline-bounded exact-PID endpoint wait: a wait
becomes TimedOut atomically when its deadline is reached. A successful endpoint
registration linearizes the capability, endpoint, and ready-wait result.
***************************************************************************)

CONSTANTS CoreServices, Pids, MaxAttempts, WaitDeadline, MaxTime

Initd == "initd"
Services == CoreServices \cup {Initd}

NoPid == 0
Absent == "absent"
Suspended == "suspended"
Running == "running"
Exited == "exited"

Idle == "idle"
Waiting == "waiting"
Ready == "ready"
Failed == "failed"
TimedOut == "timed-out"
Revoked == "revoked"

VARIABLES procState,
          procPid,
          leasePid,
          endpointPid,
          capabilityPid,
          attempts,
          waitState,
          waitPid,
          waitDeadline,
          initdAuthorized,
          issuedPids,
          now

vars == <<procState, procPid, leasePid, endpointPid, capabilityPid, attempts,
          waitState, waitPid, waitDeadline, initdAuthorized, issuedPids, now>>

Init ==
    /\ procState = [s \in Services |-> Absent]
    /\ procPid = [s \in Services |-> NoPid]
    /\ leasePid = [s \in Services |-> NoPid]
    /\ endpointPid = [s \in Services |-> NoPid]
    /\ capabilityPid = [s \in Services |-> NoPid]
    /\ attempts = [s \in Services |-> 0]
    /\ waitState = [s \in Services |-> Idle]
    /\ waitPid = [s \in Services |-> NoPid]
    /\ waitDeadline = [s \in Services |-> 0]
    /\ initdAuthorized = FALSE
    /\ issuedPids = {}
    /\ now = 0

CoreReady ==
    \A s \in CoreServices:
        /\ procState[s] = Running
        /\ leasePid[s] = procPid[s]
        /\ endpointPid[s] = procPid[s]
        /\ capabilityPid[s] = procPid[s]
        /\ waitState[s] = Ready

StartCore(s, p) ==
    /\ s \in CoreServices
    /\ p \in Pids \ issuedPids
    /\ procState[s] \in {Absent, Exited}
    /\ attempts[s] < MaxAttempts
    /\ procState' = [procState EXCEPT ![s] = Suspended]
    /\ procPid' = [procPid EXCEPT ![s] = p]
    /\ attempts' = [attempts EXCEPT ![s] = @ + 1]
    /\ waitState' = [waitState EXCEPT ![s] = Idle]
    /\ waitPid' = [waitPid EXCEPT ![s] = NoPid]
    /\ waitDeadline' = [waitDeadline EXCEPT ![s] = 0]
    /\ issuedPids' = issuedPids \cup {p}
    /\ UNCHANGED <<leasePid, endpointPid, capabilityPid, initdAuthorized, now>>

AuthorizeInitd ==
    /\ initdAuthorized = FALSE
    /\ procState[Initd] = Absent
    /\ CoreReady
    /\ initdAuthorized' = TRUE
    /\ UNCHANGED <<procState, procPid, leasePid, endpointPid, capabilityPid,
                  attempts, waitState, waitPid, waitDeadline, issuedPids, now>>

StartInitd(p) ==
    /\ p \in Pids \ issuedPids
    /\ initdAuthorized
    /\ CoreReady
    /\ procState[Initd] = Absent
    /\ attempts[Initd] = 0
    /\ procState' = [procState EXCEPT ![Initd] = Suspended]
    /\ procPid' = [procPid EXCEPT ![Initd] = p]
    /\ attempts' = [attempts EXCEPT ![Initd] = 1]
    /\ waitState' = [waitState EXCEPT ![Initd] = Idle]
    /\ waitPid' = [waitPid EXCEPT ![Initd] = NoPid]
    /\ waitDeadline' = [waitDeadline EXCEPT ![Initd] = 0]
    /\ issuedPids' = issuedPids \cup {p}
    /\ UNCHANGED <<leasePid, endpointPid, capabilityPid, initdAuthorized, now>>

AdmitLease(s) ==
    /\ s \in Services
    /\ procState[s] = Suspended
    /\ procPid[s] # NoPid
    /\ leasePid[s] = NoPid
    /\ leasePid' = [leasePid EXCEPT ![s] = procPid[s]]
    /\ UNCHANGED <<procState, procPid, endpointPid, capabilityPid, attempts,
                  waitState, waitPid, waitDeadline, initdAuthorized, issuedPids, now>>

Activate(s) ==
    /\ s \in Services
    /\ procState[s] = Suspended
    /\ procPid[s] # NoPid
    /\ leasePid[s] = procPid[s]
    /\ procState' = [procState EXCEPT ![s] = Running]
    /\ waitState' = [waitState EXCEPT ![s] = Waiting]
    /\ waitPid' = [waitPid EXCEPT ![s] = procPid[s]]
    /\ waitDeadline' = [waitDeadline EXCEPT ![s] = now + WaitDeadline]
    /\ UNCHANGED <<procPid, leasePid, endpointPid, capabilityPid, attempts,
                  initdAuthorized, issuedPids, now>>

RegisterEndpoint(s) ==
    /\ s \in Services
    /\ procState[s] = Running
    /\ waitState[s] = Waiting
    /\ now < waitDeadline[s]
    /\ endpointPid[s] = NoPid
    /\ capabilityPid[s] = NoPid
    /\ leasePid[s] = procPid[s]
    /\ endpointPid' = [endpointPid EXCEPT ![s] = procPid[s]]
    /\ capabilityPid' = [capabilityPid EXCEPT ![s] = procPid[s]]
    /\ waitState' = [waitState EXCEPT ![s] = Ready]
    /\ UNCHANGED <<procState, procPid, leasePid, attempts, waitPid,
                  waitDeadline, initdAuthorized, issuedPids, now>>

RevokeEndpoint(s) ==
    /\ s \in Services
    /\ procState[s] = Running
    /\ waitState[s] = Ready
    /\ endpointPid[s] = procPid[s]
    /\ capabilityPid[s] = procPid[s]
    /\ endpointPid' = [endpointPid EXCEPT ![s] = NoPid]
    /\ capabilityPid' = [capabilityPid EXCEPT ![s] = NoPid]
    /\ waitState' = [waitState EXCEPT ![s] = Revoked]
    /\ UNCHANGED <<procState, procPid, leasePid, attempts, waitPid,
                  waitDeadline, initdAuthorized, issuedPids, now>>

Exit(s) ==
    /\ s \in Services
    /\ procState[s] \in {Suspended, Running}
    /\ procState' = [procState EXCEPT ![s] = Exited]
    /\ procPid' = [procPid EXCEPT ![s] = NoPid]
    /\ leasePid' = [leasePid EXCEPT ![s] = NoPid]
    /\ endpointPid' = [endpointPid EXCEPT ![s] = NoPid]
    /\ capabilityPid' = [capabilityPid EXCEPT ![s] = NoPid]
    /\ waitState' = [waitState EXCEPT ![s] = Failed]
    /\ waitPid' = [waitPid EXCEPT ![s] = NoPid]
    /\ waitDeadline' = [waitDeadline EXCEPT ![s] = 0]
    /\ UNCHANGED <<attempts, initdAuthorized, issuedPids, now>>

AdvanceTime ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ waitState' =
         [s \in Services |->
            IF waitState[s] = Waiting /\ now + 1 >= waitDeadline[s]
            THEN TimedOut
            ELSE waitState[s]]
    /\ UNCHANGED <<procState, procPid, leasePid, endpointPid, capabilityPid,
                  attempts, waitPid, waitDeadline, initdAuthorized, issuedPids>>

Next ==
    \/ \E s \in CoreServices, p \in Pids : StartCore(s, p)
    \/ AuthorizeInitd
    \/ \E p \in Pids : StartInitd(p)
    \/ \E s \in Services : AdmitLease(s)
    \/ \E s \in Services : Activate(s)
    \/ \E s \in Services : RegisterEndpoint(s)
    \/ \E s \in Services : RevokeEndpoint(s)
    \/ \E s \in Services : Exit(s)
    \/ AdvanceTime

TypeOK ==
    /\ CoreServices \subseteq STRING
    /\ Initd \notin CoreServices
    /\ NoPid \notin Pids
    /\ Pids \subseteq Nat
    /\ MaxAttempts \in Nat
    /\ WaitDeadline \in Nat \ {0}
    /\ MaxTime \in Nat
    /\ procState \in [Services -> {Absent, Suspended, Running, Exited}]
    /\ procPid \in [Services -> (Pids \cup {NoPid})]
    /\ leasePid \in [Services -> (Pids \cup {NoPid})]
    /\ endpointPid \in [Services -> (Pids \cup {NoPid})]
    /\ capabilityPid \in [Services -> (Pids \cup {NoPid})]
    /\ attempts \in [Services -> 0..MaxAttempts]
    /\ waitState \in [Services -> {Idle, Waiting, Ready, Failed, TimedOut, Revoked}]
    /\ waitPid \in [Services -> (Pids \cup {NoPid})]
    /\ waitDeadline \in [Services -> Nat]
    /\ initdAuthorized \in BOOLEAN
    /\ issuedPids \subseteq Pids
    /\ now \in 0..MaxTime

LiveProcessHasLease ==
    \A s \in Services:
        procState[s] = Running =>
            /\ procPid[s] # NoPid
            /\ leasePid[s] = procPid[s]

EndpointMatchesLease ==
    \A s \in Services:
        endpointPid[s] # NoPid =>
            /\ procState[s] = Running
            /\ endpointPid[s] = procPid[s]
            /\ endpointPid[s] = leasePid[s]

CapabilityMatchesLiveEndpoint ==
    \A s \in Services:
        capabilityPid[s] # NoPid =>
            /\ procState[s] = Running
            /\ capabilityPid[s] = procPid[s]
            /\ capabilityPid[s] = leasePid[s]
            /\ capabilityPid[s] = endpointPid[s]

EndpointAndCapabilityAreAtomic ==
    \A s \in Services:
        endpointPid[s] = capabilityPid[s]

LifecycleBindingsAreConsistent ==
    /\ \A s \in Services:
        procState[s] = Absent =>
            /\ procPid[s] = NoPid
            /\ leasePid[s] = NoPid
            /\ endpointPid[s] = NoPid
            /\ capabilityPid[s] = NoPid
            /\ waitState[s] = Idle
            /\ waitPid[s] = NoPid
            /\ waitDeadline[s] = 0
    /\ \A s \in Services:
        procState[s] = Suspended =>
            /\ procPid[s] # NoPid
            /\ leasePid[s] \in {NoPid, procPid[s]}
            /\ endpointPid[s] = NoPid
            /\ capabilityPid[s] = NoPid
            /\ waitState[s] = Idle
            /\ waitPid[s] = NoPid
            /\ waitDeadline[s] = 0
    /\ \A s \in Services:
        procState[s] = Running =>
            /\ procPid[s] # NoPid
            /\ leasePid[s] = procPid[s]
            /\ waitState[s] \in {Waiting, Ready, TimedOut, Revoked}
            /\ waitPid[s] = procPid[s]
            /\ waitDeadline[s] > 0
    /\ \A s \in Services:
        procState[s] = Exited =>
            /\ procPid[s] = NoPid
            /\ leasePid[s] = NoPid
            /\ endpointPid[s] = NoPid
            /\ capabilityPid[s] = NoPid
            /\ waitState[s] = Failed
            /\ waitPid[s] = NoPid
            /\ waitDeadline[s] = 0

WaitPhaseMatchesBindings ==
    \A s \in Services:
        /\ waitState[s] = Waiting =>
            /\ endpointPid[s] = NoPid
            /\ capabilityPid[s] = NoPid
        /\ waitState[s] = Ready =>
            /\ waitPid[s] # NoPid
            /\ waitPid[s] = procPid[s]
            /\ waitPid[s] = leasePid[s]
            /\ waitPid[s] = endpointPid[s]
            /\ waitPid[s] = capabilityPid[s]
        /\ waitState[s] = TimedOut =>
            /\ endpointPid[s] = NoPid
            /\ capabilityPid[s] = NoPid
        /\ waitState[s] = Revoked =>
            /\ endpointPid[s] = NoPid
            /\ capabilityPid[s] = NoPid

WaitOutcomeRespectsDeadline ==
    /\ \A s \in Services:
        waitState[s] = Waiting => now < waitDeadline[s]
    /\ \A s \in Services:
        waitState[s] = TimedOut => now >= waitDeadline[s]

InitdWasAuthorized ==
    procState[Initd] # Absent => initdAuthorized

InitdIsSingleShot ==
    attempts[Initd] \in {0, 1}

AllNonzeroIdsWereIssued ==
    \A s \in Services:
        /\ procPid[s] # NoPid => procPid[s] \in issuedPids
        /\ leasePid[s] # NoPid => leasePid[s] \in issuedPids
        /\ endpointPid[s] # NoPid => endpointPid[s] \in issuedPids
        /\ capabilityPid[s] # NoPid => capabilityPid[s] \in issuedPids
        /\ waitPid[s] # NoPid => waitPid[s] \in issuedPids

DistinctLivePids ==
    \A s \in Services:
        \A t \in Services:
            /\ s # t
            /\ procPid[s] # NoPid
            /\ procPid[t] # NoPid
            => procPid[s] # procPid[t]

=============================================================================
