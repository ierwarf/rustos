---------------------- MODULE PostInitSupervisorRecovery ----------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models post-init service recovery after initd is replaced.

Concrete owners and source anchors:
  * lease report/query/reclaim and dependent UI revocation:
      services/rootd/src/main.rs
  * reconciliation, exact-PID endpoint adoption, and bounded stale recovery:
      services/initd/src/main.rs
  * capability-gated final process teardown substrate:
      kernel/compat/src/user/syscall/linux/lifecycle_broker_ops.rs

The model abstracts ABI bytes and PID allocation. A service has exactly one
rootd lease slot; `reporter` is the supervisor PID/generation that admitted it.
After initd crashes, the next initd must adopt only a live exact-PID endpoint.
An endpoint-less lease blocks replacement until its deadline, at which point
reclaim clears its authority and cascades to descendants reported by it.
*******************************************************************************)

CONSTANT MaxTick

Services == {"netd", "sessiond", "uiserver"}
InitdManaged == {"netd", "sessiond"}
Empty == "empty"
Admitted == "admitted"
Ready == "ready"
Exited == "exited"
LeaseStates == {Empty, Admitted, Ready, Exited}

Dependents(service) == IF service = "sessiond" THEN {"uiserver"} ELSE {}
Cascade(service) == {service} \cup Dependents(service)

VARIABLES currentInitd, state, live, endpoint, reporter, recovery, deadline,
          tracked, clock

vars == <<currentInitd, state, live, endpoint, reporter, recovery, deadline,
          tracked, clock>>

Init ==
    /\ currentInitd = "old"
    /\ state = [service \in Services |-> Ready]
    /\ live = [service \in Services |-> TRUE]
    /\ endpoint = [service \in Services |-> TRUE]
    /\ reporter = [service \in Services |-> "old"]
    /\ recovery = [service \in Services |-> FALSE]
    /\ deadline = [service \in Services |-> 0]
    /\ tracked = [service \in Services |-> TRUE]
    /\ clock = 0

\* Initd exits while its reported children remain. The new initd deliberately
\* knows none of them until it checks the exact-PID endpoint.
CrashAndReplaceInitd ==
    /\ currentInitd = "old"
    /\ currentInitd' = "new"
    /\ recovery' = [service \in Services |->
          service \in InitdManaged /\ state[service] # Empty]
    /\ deadline' = [service \in Services |->
          IF service \in InitdManaged /\ state[service] # Empty
          THEN clock + 2 ELSE deadline[service]]
    /\ tracked' = [service \in Services |->
          IF service \in InitdManaged /\ state[service] # Empty
          THEN FALSE ELSE tracked[service]]
    /\ UNCHANGED <<state, live, endpoint, reporter, clock>>

\* A child can still finish endpoint registration after its old supervisor
\* dies. That is safe only because adoption validates the lease's exact PID.
LateEndpointReady(service) ==
    /\ service \in InitdManaged
    /\ recovery[service]
    /\ state[service] = Admitted
    /\ live[service]
    /\ state' = [state EXCEPT ![service] = Ready]
    /\ endpoint' = [endpoint EXCEPT ![service] = TRUE]
    /\ UNCHANGED <<currentInitd, live, reporter, recovery, deadline, tracked, clock>>

AdoptExactReadyLease(service) ==
    /\ service \in InitdManaged
    /\ currentInitd = "new"
    /\ recovery[service]
    /\ state[service] = Ready
    /\ live[service]
    /\ endpoint[service]
    /\ recovery' = [recovery EXCEPT ![service] = FALSE]
    /\ tracked' = [tracked EXCEPT ![service] = TRUE]
    /\ UNCHANGED <<currentInitd, state, live, endpoint, reporter, deadline, clock>>

\* A normally observed service exit is also reconciled through rootd before a
\* replacement is attempted. sessiond's UI child loses authority immediately.
ObserveExit(service) ==
    /\ service \in InitdManaged
    /\ state[service] \in {Admitted, Ready}
    /\ state' = [x \in Services |-> IF x \in Cascade(service) THEN Exited ELSE state[x]]
    /\ live' = [x \in Services |-> IF x \in Cascade(service) THEN FALSE ELSE live[x]]
    /\ endpoint' = [x \in Services |-> IF x \in Cascade(service) THEN FALSE ELSE endpoint[x]]
    /\ recovery' = [recovery EXCEPT ![service] = TRUE]
    /\ deadline' = [deadline EXCEPT ![service] = clock]
    /\ tracked' = [tracked EXCEPT ![service] = FALSE]
    /\ UNCHANGED <<currentInitd, reporter, clock>>

\* Deadline expiry permits rootd's complete teardown. It may never reclaim a
\* ready exact-PID lease: that lease must be adopted instead.
ReclaimStaleLease(service) ==
    /\ service \in InitdManaged
    /\ recovery[service]
    /\ clock >= deadline[service]
    /\ state[service] \in {Admitted, Exited}
    /\ state' = [x \in Services |-> IF x \in Cascade(service) THEN Empty ELSE state[x]]
    /\ live' = [x \in Services |-> IF x \in Cascade(service) THEN FALSE ELSE live[x]]
    /\ endpoint' = [x \in Services |-> IF x \in Cascade(service) THEN FALSE ELSE endpoint[x]]
    /\ reporter' = [x \in Services |-> IF x \in Cascade(service) THEN "none" ELSE reporter[x]]
    /\ recovery' = [recovery EXCEPT ![service] = FALSE]
    /\ tracked' = [tracked EXCEPT ![service] = FALSE]
    /\ UNCHANGED <<currentInitd, deadline, clock>>

LaunchReplacement(service) ==
    /\ service \in InitdManaged
    /\ state[service] = Empty
    /\ ~recovery[service]
    \* The concrete deferred-start transaction does not return to initd's
    \* main loop until exact-PID endpoint readiness succeeds, so this action
    \* abstracts that whole bounded transaction atomically.
    /\ state' = [state EXCEPT ![service] = Ready]
    /\ live' = [live EXCEPT ![service] = TRUE]
    /\ endpoint' = [endpoint EXCEPT ![service] = TRUE]
    /\ reporter' = [reporter EXCEPT ![service] = currentInitd]
    /\ tracked' = [tracked EXCEPT ![service] = TRUE]
    /\ UNCHANGED <<currentInitd, recovery, deadline, clock>>

\* Time may reach a recovery deadline but cannot pass it. At that boundary a
\* stale lease must be reclaimed, or (if ready) adopted; it cannot be ignored.
AdvanceClock ==
    /\ clock < MaxTick
    /\ \A service \in InitdManaged:
          recovery[service] => clock + 1 <= deadline[service]
    /\ clock' = clock + 1
    /\ UNCHANGED <<currentInitd, state, live, endpoint, reporter, recovery,
                  deadline, tracked>>

Next ==
    \/ CrashAndReplaceInitd
    \/ \E service \in InitdManaged: LateEndpointReady(service)
    \/ \E service \in InitdManaged: AdoptExactReadyLease(service)
    \/ \E service \in InitdManaged: ObserveExit(service)
    \/ \E service \in InitdManaged: ReclaimStaleLease(service)
    \/ \E service \in InitdManaged: LaunchReplacement(service)
    \/ AdvanceClock

TypeOK ==
    /\ currentInitd \in {"old", "new"}
    /\ state \in [Services -> LeaseStates]
    /\ live \in [Services -> BOOLEAN]
    /\ endpoint \in [Services -> BOOLEAN]
    /\ reporter \in [Services -> {"old", "new", "none"}]
    /\ recovery \in [Services -> BOOLEAN]
    /\ deadline \in [Services -> 0..(MaxTick + 2)]
    /\ tracked \in [Services -> BOOLEAN]
    /\ clock \in 0..MaxTick

EndpointImpliesLiveAdmittedLease ==
    \A service \in Services:
        endpoint[service] => live[service] /\ state[service] = Ready

EmptyLeaseHasNoProcessOrAuthority ==
    \A service \in Services:
        state[service] = Empty =>
            /\ ~live[service]
            /\ ~endpoint[service]
            /\ reporter[service] = "none"

RecoveryBlocksDuplicateLaunch ==
    \A service \in InitdManaged: recovery[service] => state[service] # Empty

TrackedServiceHasExactReadyEndpoint ==
    \A service \in InitdManaged:
        tracked[service] =>
            /\ state[service] = Ready
            /\ live[service]
            /\ endpoint[service]

DeadSessionCannotRetainUiAuthority ==
    state["sessiond"] # Ready =>
        /\ state["uiserver"] # Ready
        /\ ~endpoint["uiserver"]

ReadyRecoveryHasExactEndpoint ==
    \A service \in InitdManaged:
        recovery[service] /\ state[service] = Ready =>
            live[service] /\ endpoint[service]

Spec == Init /\ [][Next]_vars
================================================================================
