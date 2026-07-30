--------------------- MODULE FutexWaiterLifecycle ---------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Owner: kernel/compat Linux futex scheduler substrate.
Linearization points: local opcode/flag admission, waiter-table insertion,
wake removal, requeue key replacement, scheduler retirement publication,
robust-list snapshot, and the exact task-identity cleanup acknowledgement that
permits slot reuse. Supported futex operations are admitted by the ring0
scheduler substrate without a synchronous policy-service round trip: a caller
must snapshot its current task/MM binding from scheduler-local state and must
not be exposed to a contended process-state lock, service scheduling, restart,
or IPC timeout before its waiter and deadline can be armed. The
captured original key is intentionally retained so requeue followed by timeout
is explored. Robust cleanup is bounded, includes the pending operation, marks
only words still owned by the retiring task, and removes the task's user-space
ownership before the scheduler slot may be reused.
***************************************************************************)

CONSTANTS Tasks, Keys, MaxWaiters, MaxRobustEntries
NoKey == "none"
NoOwner == "no-owner"
NoOutcome == "none"
Woken == "woken"
TimedOut == "timed-out"
Spurious == "spurious"
Retiring == "retiring"
Exited == "exited"
VARIABLES alive, retiring, retired, blocked, locallyAdmitted, waiterKey, originalKey,
          completed, outcome,
          robustList, robustPending, robustOwner, robustWaiters, ownerDied,
          ownedAtRetire, cleanedOwned
vars == <<alive, retiring, retired, blocked, locallyAdmitted, waiterKey, originalKey,
          completed, outcome,
          robustList, robustPending, robustOwner, robustWaiters, ownerDied,
          ownedAtRetire, cleanedOwned>>

Init ==
    /\ alive = Tasks /\ retiring = {} /\ retired = {} /\ blocked = {}
    /\ locallyAdmitted = {}
    /\ waiterKey = [t \in Tasks |-> NoKey]
    /\ originalKey = [t \in Tasks |-> NoKey]
    /\ completed = {}
    /\ outcome = [t \in Tasks |-> NoOutcome]
    /\ robustList = [t \in Tasks |-> {}]
    /\ robustPending = [t \in Tasks |-> NoKey]
    /\ robustOwner = [k \in Keys |-> NoOwner]
    /\ robustWaiters = {}
    /\ ownerDied = {}
    /\ ownedAtRetire = [t \in Tasks |-> {}]
    /\ cleanedOwned = [t \in Tasks |-> {}]

AdmitLocally(t) ==
    /\ t \in alive
    /\ waiterKey[t] = NoKey
    /\ locallyAdmitted' = locallyAdmitted \cup {t}
    /\ UNCHANGED <<alive, retiring, retired, blocked, waiterKey, originalKey,
                    completed, outcome, robustList, robustPending, robustOwner,
                    robustWaiters, ownerDied, ownedAtRetire, cleanedOwned>>

Register(t, k) ==
    /\ t \in alive /\ t \in locallyAdmitted /\ waiterKey[t] = NoKey
    /\ Cardinality({u \in Tasks: waiterKey[u] # NoKey}) < MaxWaiters
    /\ waiterKey' = [waiterKey EXCEPT ![t] = k]
    /\ originalKey' = [originalKey EXCEPT ![t] = k]
    /\ blocked' = blocked \cup {t} /\ completed' = completed \ {t}
    /\ outcome' = [outcome EXCEPT ![t] = NoOutcome]
    /\ UNCHANGED <<alive, retiring, retired, locallyAdmitted,
                    robustList, robustPending,
                    robustOwner, robustWaiters, ownerDied, ownedAtRetire,
                    cleanedOwned>>

Requeue(t, k) ==
    /\ t \in blocked /\ waiterKey[t] # NoKey /\ k \in Keys
    /\ waiterKey' = [waiterKey EXCEPT ![t] = k]
    /\ UNCHANGED <<alive, retiring, retired, blocked, locallyAdmitted,
                    originalKey, completed, outcome,
                    robustList, robustPending, robustOwner, robustWaiters,
                    ownerDied, ownedAtRetire, cleanedOwned>>

Finish(t, result) ==
    /\ t \in blocked
    /\ result \in {Woken, TimedOut, Spurious}
    /\ waiterKey' = [waiterKey EXCEPT ![t] = NoKey]
    /\ originalKey' = [originalKey EXCEPT ![t] = NoKey]
    /\ blocked' = blocked \ {t} /\ completed' = completed \cup {t}
    /\ locallyAdmitted' = locallyAdmitted \ {t}
    /\ outcome' = [outcome EXCEPT ![t] = result]
    /\ UNCHANGED <<alive, retiring, retired, robustList, robustPending,
                    robustOwner, robustWaiters, ownerDied, ownedAtRetire,
                    cleanedOwned>>

Wake(t, key) == /\ waiterKey[t] = key /\ Finish(t, Woken)
Timeout(t) == Finish(t, TimedOut)
SpuriousWake(t) == Finish(t, Spurious)

PublishRobustList(t, k) ==
    /\ t \in alive
    /\ robustList[t] = {}
    /\ 1 <= MaxRobustEntries
    /\ robustList' = [robustList EXCEPT ![t] = {k}]
    /\ UNCHANGED robustPending
    /\ UNCHANGED <<alive, retiring, retired, blocked, locallyAdmitted,
                    waiterKey, originalKey,
                    completed, outcome, robustOwner, robustWaiters, ownerDied,
                    ownedAtRetire, cleanedOwned>>

BeginRobustOperation(t, k) ==
    /\ t \in alive
    /\ robustList[t] # {}
    /\ robustPending[t] = NoKey
    /\ robustPending' = [robustPending EXCEPT ![t] = k]
    /\ UNCHANGED <<alive, retiring, retired, blocked, locallyAdmitted,
                    waiterKey, originalKey,
                    completed, outcome, robustList, robustOwner, robustWaiters,
                    ownerDied, ownedAtRetire, cleanedOwned>>

AcquireRobustWord(t, k, hasWaiter) ==
    /\ t \in alive
    /\ k \in robustList[t] \/ k = robustPending[t]
    /\ robustOwner[k] = NoOwner
    /\ robustOwner' = [robustOwner EXCEPT ![k] = t]
    /\ robustWaiters' = IF hasWaiter
                          THEN robustWaiters \cup {k}
                          ELSE robustWaiters \ {k}
    /\ ownerDied' = ownerDied \ {k}
    /\ UNCHANGED <<alive, retiring, retired, blocked, locallyAdmitted,
                    waiterKey, originalKey,
                    completed, outcome, robustList, robustPending, ownedAtRetire,
                    cleanedOwned>>

RequestRetire(t) ==
    /\ t \in alive
    /\ alive' = alive \ {t}
    /\ retiring' = retiring \cup {t}
    /\ completed' = completed \ {t}
    /\ outcome' = [outcome EXCEPT ![t] = Retiring]
    /\ ownedAtRetire' = [ownedAtRetire EXCEPT ![t] =
           {k \in robustList[t] \cup
                    (IF robustPending[t] = NoKey THEN {} ELSE {robustPending[t]}):
                robustOwner[k] = t}]
    /\ UNCHANGED <<retired, blocked, locallyAdmitted, waiterKey, originalKey,
                    robustList,
                    robustPending, robustOwner, robustWaiters, ownerDied,
                    cleanedOwned>>

CleanupRetired(t) ==
    /\ t \in retiring
    /\ retiring' = retiring \ {t}
    /\ retired' = retired \cup {t}
    /\ waiterKey' = [waiterKey EXCEPT ![t] = NoKey]
    /\ originalKey' = [originalKey EXCEPT ![t] = NoKey]
    /\ blocked' = blocked \ {t} /\ completed' = completed \cup {t}
    /\ locallyAdmitted' = locallyAdmitted \ {t}
    /\ outcome' = [outcome EXCEPT ![t] = Exited]
    /\ robustOwner' = [k \in Keys |->
           IF k \in ownedAtRetire[t] THEN NoOwner ELSE robustOwner[k]]
    /\ robustWaiters' = robustWaiters \ ownedAtRetire[t]
    /\ ownerDied' = ownerDied \cup ownedAtRetire[t]
    /\ robustList' = [robustList EXCEPT ![t] = {}]
    /\ robustPending' = [robustPending EXCEPT ![t] = NoKey]
    /\ cleanedOwned' = [cleanedOwned EXCEPT ![t] = ownedAtRetire[t]]
    /\ UNCHANGED <<alive, ownedAtRetire>>

Next ==
    \/ \E t \in Tasks: AdmitLocally(t)
    \/ \E t \in Tasks, k \in Keys: Register(t, k) \/ Requeue(t, k)
    \/ \E t \in Tasks, k \in Keys: Wake(t, k)
    \/ \E t \in Tasks: Timeout(t) \/ SpuriousWake(t)
                            \/ RequestRetire(t) \/ CleanupRetired(t)
    \/ \E t \in Tasks, k \in Keys:
           PublishRobustList(t, k) \/ BeginRobustOperation(t, k)
    \/ \E t \in Tasks, k \in Keys, hasWaiter \in BOOLEAN:
           AcquireRobustWord(t, k, hasWaiter)
Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ \A t \in Tasks: WF_vars(CleanupRetired(t))

TypeOK ==
    /\ alive \in SUBSET Tasks /\ retiring \in SUBSET Tasks
    /\ retired \in SUBSET Tasks
    /\ blocked \in SUBSET Tasks
    /\ locallyAdmitted \in SUBSET Tasks
    /\ completed \in SUBSET Tasks
    /\ waiterKey \in [Tasks -> Keys \cup {NoKey}]
    /\ originalKey \in [Tasks -> Keys \cup {NoKey}]
    /\ outcome \in [Tasks -> {NoOutcome, Woken, TimedOut, Spurious, Retiring, Exited}]
    /\ robustList \in [Tasks -> SUBSET Keys]
    /\ robustPending \in [Tasks -> Keys \cup {NoKey}]
    /\ robustOwner \in [Keys -> Tasks \cup {NoOwner}]
    /\ robustWaiters \in SUBSET Keys
    /\ ownerDied \in SUBSET Keys
    /\ ownedAtRetire \in [Tasks -> SUBSET Keys]
    /\ cleanedOwned \in [Tasks -> SUBSET Keys]
OneWaiterPerTask == Cardinality({t \in Tasks: waiterKey[t] # NoKey}) <= MaxWaiters
RobustListIsBounded ==
    \A t \in Tasks: Cardinality(robustList[t]) <= MaxRobustEntries
LifecycleSetsDisjoint ==
    /\ alive \intersect retiring = {}
    /\ alive \intersect retired = {}
    /\ retiring \intersect retired = {}
WaiterIsOwnedAndBlocked ==
    \A t \in Tasks: waiterKey[t] # NoKey =>
        t \in (alive \union retiring) /\ t \in blocked
WaiterWasLocallyAdmitted ==
    \A t \in Tasks: waiterKey[t] # NoKey => t \in locallyAdmitted
BlockedExactlyMatchesWaiters == \A t \in Tasks: (t \in blocked) <=> (waiterKey[t] # NoKey)
WaiterRetainsOriginalIdentity ==
    \A t \in blocked: originalKey[t] # NoKey /\ outcome[t] \in {NoOutcome, Retiring}
TerminalTaskHasNoWaitAuthority ==
    \A t \in Tasks: (t \in retired) <=>
        (waiterKey[t] = NoKey /\ originalKey[t] = NoKey /\
         t \notin locallyAdmitted /\ outcome[t] = Exited)
TerminalTaskHasNoRobustAuthority ==
    \A t \in retired:
        /\ robustList[t] = {}
        /\ robustPending[t] = NoKey
        /\ cleanedOwned[t] = ownedAtRetire[t]
OwnerDeathWakesWaiters ==
    \A k \in ownerDied: k \notin robustWaiters /\ robustOwner[k] = NoOwner
RetirementCleanupSettles == \A t \in Tasks: t \in retiring ~> t \in retired

=============================================================================
