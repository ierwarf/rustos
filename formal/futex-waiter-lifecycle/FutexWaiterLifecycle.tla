--------------------- MODULE FutexWaiterLifecycle ---------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Owner: kernel/compat Linux futex scheduler substrate.
Linearization points: waiter-table insertion, wake removal, requeue key
replacement, and task-identity cleanup on timeout/exit. The captured original
key is intentionally retained so requeue followed by timeout is explored.
***************************************************************************)

CONSTANTS Tasks, Keys, MaxWaiters
NoKey == "none"
NoOutcome == "none"
Woken == "woken"
TimedOut == "timed-out"
Spurious == "spurious"
Exited == "exited"
VARIABLES alive, blocked, waiterKey, originalKey, completed, outcome
vars == <<alive, blocked, waiterKey, originalKey, completed, outcome>>

Init ==
    /\ alive = Tasks /\ blocked = {}
    /\ waiterKey = [t \in Tasks |-> NoKey]
    /\ originalKey = [t \in Tasks |-> NoKey]
    /\ completed = {}
    /\ outcome = [t \in Tasks |-> NoOutcome]

Register(t, k) ==
    /\ t \in alive /\ waiterKey[t] = NoKey
    /\ Cardinality({u \in Tasks: waiterKey[u] # NoKey}) < MaxWaiters
    /\ waiterKey' = [waiterKey EXCEPT ![t] = k]
    /\ originalKey' = [originalKey EXCEPT ![t] = k]
    /\ blocked' = blocked \cup {t} /\ completed' = completed \ {t}
    /\ outcome' = [outcome EXCEPT ![t] = NoOutcome]
    /\ UNCHANGED alive

Requeue(t, k) ==
    /\ t \in blocked /\ waiterKey[t] # NoKey /\ k \in Keys
    /\ waiterKey' = [waiterKey EXCEPT ![t] = k]
    /\ UNCHANGED <<alive, blocked, originalKey, completed, outcome>>

Finish(t, result) ==
    /\ t \in blocked
    /\ result \in {Woken, TimedOut, Spurious}
    /\ waiterKey' = [waiterKey EXCEPT ![t] = NoKey]
    /\ originalKey' = [originalKey EXCEPT ![t] = NoKey]
    /\ blocked' = blocked \ {t} /\ completed' = completed \cup {t}
    /\ outcome' = [outcome EXCEPT ![t] = result]
    /\ UNCHANGED alive

Wake(t, key) == /\ waiterKey[t] = key /\ Finish(t, Woken)
Timeout(t) == Finish(t, TimedOut)
SpuriousWake(t) == Finish(t, Spurious)

Exit(t) ==
    /\ t \in alive
    /\ alive' = alive \ {t}
    /\ waiterKey' = [waiterKey EXCEPT ![t] = NoKey]
    /\ originalKey' = [originalKey EXCEPT ![t] = NoKey]
    /\ blocked' = blocked \ {t} /\ completed' = completed \cup {t}
    /\ outcome' = [outcome EXCEPT ![t] = Exited]

Next ==
    \/ \E t \in Tasks, k \in Keys: Register(t, k) \/ Requeue(t, k)
    \/ \E t \in Tasks, k \in Keys: Wake(t, k)
    \/ \E t \in Tasks: Timeout(t) \/ SpuriousWake(t) \/ Exit(t)
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ alive \in SUBSET Tasks /\ blocked \in SUBSET Tasks
    /\ completed \in SUBSET Tasks
    /\ waiterKey \in [Tasks -> Keys \cup {NoKey}]
    /\ originalKey \in [Tasks -> Keys \cup {NoKey}]
    /\ outcome \in [Tasks -> {NoOutcome, Woken, TimedOut, Spurious, Exited}]
OneWaiterPerTask == Cardinality({t \in Tasks: waiterKey[t] # NoKey}) <= MaxWaiters
WaiterIsLiveAndBlocked == \A t \in Tasks: waiterKey[t] # NoKey => t \in alive /\ t \in blocked
BlockedExactlyMatchesWaiters == \A t \in Tasks: (t \in blocked) <=> (waiterKey[t] # NoKey)
WaiterRetainsOriginalIdentity == \A t \in blocked: originalKey[t] # NoKey /\ outcome[t] = NoOutcome
TerminalTaskHasNoWaitAuthority ==
    \A t \in Tasks: (t \in completed) <=>
        (waiterKey[t] = NoKey /\ originalKey[t] = NoKey /\ outcome[t] # NoOutcome)

=============================================================================
