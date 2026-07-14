------------------------------ MODULE EndpointRegistry ------------------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Models the externally observable IPC service registry. Endpoint publication is
the registration commit point: an effective capability and an exact-PID wait
may succeed only after the service endpoint is visible. Revoke and process
exit fail closed before stale bindings can authorize a broker.
***************************************************************************)

CONSTANTS Services, Pids, Waiters, WaitTimeout, MaxTime

NoPid == 0
NoService == "none"
Absent == "absent"
Running == "running"
Exited == "exited"

Idle == "idle"
Waiting == "waiting"
Ready == "ready"
TimedOut == "timed-out"
WaitExited == "exited"
Rejected == "rejected"

VARIABLES procState,
          leasePid,
          endpointPid,
          capabilityPid,
          waitService,
          waitPid,
          waitState,
          waitResultPid,
          waitDeadline,
          issuedPids,
          now

vars == <<procState, leasePid, endpointPid, capabilityPid, waitService,
          waitPid, waitState, waitResultPid, waitDeadline, issuedPids, now>>

Init ==
    /\ procState = [s \in Services |-> Absent]
    /\ leasePid = [s \in Services |-> NoPid]
    /\ endpointPid = [s \in Services |-> NoPid]
    /\ capabilityPid = [s \in Services |-> NoPid]
    /\ waitService = [w \in Waiters |-> NoService]
    /\ waitPid = [w \in Waiters |-> NoPid]
    /\ waitState = [w \in Waiters |-> Idle]
    /\ waitResultPid = [w \in Waiters |-> NoPid]
    /\ waitDeadline = [w \in Waiters |-> 0]
    /\ issuedPids = {}
    /\ now = 0

EndpointMatchesWaiter(w) ==
    /\ endpointPid[waitService[w]] = waitPid[w]
    /\ capabilityPid[waitService[w]] = waitPid[w]
    /\ waitPid[w] # NoPid

ExpectedProcessRunning(w) ==
    \E s \in Services:
        /\ procState[s] = Running
        /\ leasePid[s] = waitPid[w]

Spawn(s, p) ==
    /\ s \in Services
    /\ p \in Pids \ issuedPids
    /\ procState[s] \in {Absent, Exited}
    /\ procState' = [procState EXCEPT ![s] = Running]
    /\ leasePid' = [leasePid EXCEPT ![s] = p]
    /\ issuedPids' = issuedPids \cup {p}
    /\ UNCHANGED <<endpointPid, capabilityPid, waitService, waitPid, waitState,
                  waitResultPid, waitDeadline, now>>

Publish(s) ==
    /\ s \in Services
    /\ procState[s] = Running
    /\ leasePid[s] # NoPid
    /\ endpointPid[s] = NoPid
    /\ capabilityPid[s] = NoPid
    /\ endpointPid' = [endpointPid EXCEPT ![s] = leasePid[s]]
    /\ capabilityPid' = [capabilityPid EXCEPT ![s] = leasePid[s]]
    /\ UNCHANGED <<procState, leasePid, waitService, waitPid, waitState,
                  waitResultPid, waitDeadline, issuedPids, now>>

Revoke(s) ==
    /\ s \in Services
    /\ procState[s] = Running
    /\ endpointPid[s] = leasePid[s]
    /\ capabilityPid[s] = leasePid[s]
    /\ endpointPid' = [endpointPid EXCEPT ![s] = NoPid]
    /\ capabilityPid' = [capabilityPid EXCEPT ![s] = NoPid]
    /\ UNCHANGED <<procState, leasePid, waitService, waitPid, waitState,
                  waitResultPid, waitDeadline, issuedPids, now>>

Exit(s) ==
    /\ s \in Services
    /\ procState[s] = Running
    /\ LET departed == leasePid[s] IN
       /\ procState' = [procState EXCEPT ![s] = Exited]
       /\ leasePid' = [leasePid EXCEPT ![s] = NoPid]
       /\ endpointPid' = [endpointPid EXCEPT ![s] = NoPid]
       /\ capabilityPid' = [capabilityPid EXCEPT ![s] = NoPid]
       /\ waitState' =
            [w \in Waiters |->
                IF waitState[w] = Waiting /\ waitService[w] = s /\ waitPid[w] = departed
                THEN WaitExited
                ELSE waitState[w]]
       /\ waitDeadline' =
            [w \in Waiters |->
                IF waitState[w] = Waiting /\ waitService[w] = s /\ waitPid[w] = departed
                THEN 0
                ELSE waitDeadline[w]]
       /\ UNCHANGED <<waitService, waitPid, waitResultPid, issuedPids, now>>

BeginWait(w, s, p) ==
    /\ w \in Waiters
    /\ s \in Services
    /\ p \in Pids
    /\ waitState[w] = Idle
    \* An immediate Ready/WaitExited result is valid at the final tick; only
    \* a real pending wait needs enough bounded model time to expire.
    /\ (procState[s] = Running /\ leasePid[s] = p
        /\ ~(endpointPid[s] = p /\ capabilityPid[s] = p))
        => now <= MaxTime - WaitTimeout
    /\ waitService' = [waitService EXCEPT ![w] = s]
    /\ waitPid' = [waitPid EXCEPT ![w] = p]
    /\ waitState' =
         [waitState EXCEPT ![w] =
            IF endpointPid[s] = p /\ capabilityPid[s] = p
            THEN Ready
            ELSE IF procState[s] = Running /\ leasePid[s] = p
                 THEN Waiting
                 ELSE WaitExited]
    /\ waitResultPid' =
         [waitResultPid EXCEPT ![w] =
            IF endpointPid[s] = p /\ capabilityPid[s] = p THEN p ELSE NoPid]
    /\ waitDeadline' =
         [waitDeadline EXCEPT ![w] =
            IF procState[s] = Running /\ leasePid[s] = p
               /\ ~(endpointPid[s] = p /\ capabilityPid[s] = p)
            THEN now + WaitTimeout
            ELSE 0]
    /\ UNCHANGED <<procState, leasePid, endpointPid, capabilityPid, issuedPids, now>>

AdvanceTime ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ waitState' =
         [w \in Waiters |->
            IF waitState[w] = Waiting /\ EndpointMatchesWaiter(w)
            THEN Ready
            ELSE IF waitState[w] = Waiting /\ ~ExpectedProcessRunning(w)
                 THEN WaitExited
                 ELSE IF waitState[w] = Waiting /\ now + 1 >= waitDeadline[w]
                      THEN TimedOut
                      ELSE waitState[w]]
    /\ waitResultPid' =
         [w \in Waiters |->
            IF waitState[w] = Waiting /\ EndpointMatchesWaiter(w)
            THEN waitPid[w]
            ELSE waitResultPid[w]]
    /\ waitDeadline' =
         [w \in Waiters |->
            IF waitState[w] = Waiting
               /\ (EndpointMatchesWaiter(w)
                   \/ ~ExpectedProcessRunning(w)
                   \/ now + 1 >= waitDeadline[w])
            THEN 0
            ELSE waitDeadline[w]]
    /\ UNCHANGED <<procState, leasePid, endpointPid, capabilityPid, waitService,
                  waitPid, issuedPids>>

Next ==
    \/ \E s \in Services, p \in Pids : Spawn(s, p)
    \/ \E s \in Services : Publish(s)
    \/ \E s \in Services : Revoke(s)
    \/ \E s \in Services : Exit(s)
    \/ \E w \in Waiters, s \in Services, p \in Pids : BeginWait(w, s, p)
    \/ AdvanceTime

TypeOK ==
    /\ Services \subseteq STRING
    /\ Pids \subseteq Nat
    /\ NoPid \notin Pids
    /\ WaitTimeout \in Nat \ {0}
    /\ MaxTime \in Nat
    /\ procState \in [Services -> {Absent, Running, Exited}]
    /\ leasePid \in [Services -> (Pids \cup {NoPid})]
    /\ endpointPid \in [Services -> (Pids \cup {NoPid})]
    /\ capabilityPid \in [Services -> (Pids \cup {NoPid})]
    /\ waitService \in [Waiters -> (Services \cup {NoService})]
    /\ waitPid \in [Waiters -> (Pids \cup {NoPid})]
    /\ waitState \in [Waiters -> {Idle, Waiting, Ready, TimedOut, WaitExited, Rejected}]
    /\ waitResultPid \in [Waiters -> (Pids \cup {NoPid})]
    /\ waitDeadline \in [Waiters -> 0..MaxTime]
    /\ issuedPids \subseteq Pids
    /\ now \in 0..MaxTime

PublishedEndpointNeedsLiveLease ==
    \A s \in Services:
        endpointPid[s] # NoPid =>
            /\ procState[s] = Running
            /\ endpointPid[s] = leasePid[s]

CapabilityNeedsPublishedEndpoint ==
    \A s \in Services:
        capabilityPid[s] # NoPid =>
            /\ capabilityPid[s] = endpointPid[s]
            /\ capabilityPid[s] = leasePid[s]
            /\ procState[s] = Running

EndpointAndCapabilityAreAtomic ==
    \A s \in Services:
        endpointPid[s] = capabilityPid[s]

LifecycleClearsBindings ==
    \A s \in Services:
        procState[s] \in {Absent, Exited} =>
            /\ leasePid[s] = NoPid
            /\ endpointPid[s] = NoPid
            /\ capabilityPid[s] = NoPid

WaitStateIsWellFormed ==
    \A w \in Waiters:
        /\ waitState[w] = Idle =>
            /\ waitService[w] = NoService
            /\ waitPid[w] = NoPid
            /\ waitResultPid[w] = NoPid
            /\ waitDeadline[w] = 0
        /\ waitState[w] = Waiting =>
            /\ waitService[w] \in Services
            /\ waitPid[w] # NoPid
            /\ waitResultPid[w] = NoPid
            /\ waitDeadline[w] > now
            /\ ExpectedProcessRunning(w)
        /\ waitState[w] = Ready =>
            /\ waitService[w] \in Services
            /\ waitPid[w] # NoPid
            /\ waitResultPid[w] = waitPid[w]
            /\ waitDeadline[w] = 0
        /\ waitState[w] \in {TimedOut, WaitExited, Rejected} =>
            /\ waitService[w] \in Services
            /\ waitPid[w] # NoPid
            /\ waitResultPid[w] = NoPid
            /\ waitDeadline[w] = 0

ReadyWaitHasIssuedExactPid ==
    \A w \in Waiters:
        waitState[w] = Ready =>
            /\ waitResultPid[w] \in issuedPids
            /\ waitResultPid[w] = waitPid[w]

WaitEventuallySettles ==
    \A w \in Waiters:
        waitState[w] = Waiting ~> waitState[w] # Waiting

AllLiveBindingsUseIssuedPids ==
    \A s \in Services:
        /\ leasePid[s] # NoPid => leasePid[s] \in issuedPids
        /\ endpointPid[s] # NoPid => endpointPid[s] \in issuedPids
        /\ capabilityPid[s] # NoPid => capabilityPid[s] \in issuedPids

DistinctLiveServicePids ==
    \A s \in Services:
        \A t \in Services:
            /\ s # t
            /\ leasePid[s] # NoPid
            /\ leasePid[t] # NoPid
            => leasePid[s] # leasePid[t]

Spec == Init /\ [][Next]_vars /\ WF_vars(AdvanceTime)

=============================================================================
