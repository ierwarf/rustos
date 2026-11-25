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
Supervisor death degrades the tree instead of cascading through it. An observed
initd exit does NOT kill what initd reported: those leases stay live, are fenced
from broker authority (their declared reporter is dead) and get a bounded
adoption grace (`ORPHAN_ADOPTION_GRACE_NS` in rootd). A replacement initd adopts
only a live exact-PID endpoint and rebinds the reporter; a lease nobody adopts
by its deadline is reclaimed and the reclaim cascades to descendants reported
by it. A slow initd->rootd query never ends initd (services/initd
lease_reconciliation.rs): the same recovery window simply keeps running.
A non-initd reporter (sessiond) exiting still revokes its dependents at once.
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
    /\ currentInitd \in {"old", "new"}
    \* Recovery may start at any cut through the old supervisor's bounded
    \* admit -> endpoint-ready transaction.  Restricting Init to Ready made
    \* LateEndpointReady dead code and silently omitted the crash race that
    \* this model exists to check.
    /\ state \in [Services -> {Admitted, Ready}]
    /\ state["sessiond"] # Ready => state["uiserver"] # Ready
    /\ live = [service \in Services |-> TRUE]
    /\ endpoint = [service \in Services |-> state[service] = Ready]
    /\ reporter = [service \in Services |-> "old"]
    /\ recovery = [service \in Services |->
          currentInitd = "new" /\ service \in InitdManaged /\ state[service] # Empty]
    /\ deadline = [service \in Services |->
          IF currentInitd = "new" /\ service \in InitdManaged /\ state[service] # Empty
          THEN 2 ELSE 0]
    /\ tracked = [service \in Services |->
          currentInitd = "old" /\ state[service] = Ready]
    /\ clock = 0

\* Rootd observes initd exit before it starts the replacement. Nothing is
\* killed: every service keeps running but is fenced (reporter "old" is dead
\* once currentInitd = "new") and an adoption deadline is armed.
CrashAndReplaceInitd ==
    /\ currentInitd = "old"
    \* Every post-crash adoption/reclaim window must fit inside the finite
    \* TLC clock; otherwise a final-tick replacement would hide the deadline.
    /\ clock <= MaxTick - 2
    /\ currentInitd' = "new"
    /\ recovery' = [service \in Services |->
          service \in InitdManaged /\ state[service] # Empty]
    /\ deadline' = [service \in Services |->
          IF service \in InitdManaged /\ state[service] # Empty
          THEN clock + 2 ELSE deadline[service]]
    /\ tracked' = [service \in Services |->
          IF service \in InitdManaged THEN FALSE ELSE tracked[service]]
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
    \* rootd rebinds the exact live lease to the querying (current) initd.
    /\ reporter' = [reporter EXCEPT ![service] = "new"]
    /\ UNCHANGED <<currentInitd, state, live, endpoint, deadline, clock>>

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

\* Deadline expiry permits rootd's complete teardown, including of an orphaned
\* Ready lease nobody adopted in time (a service must not run unsupervised and
\* unfenced forever). Before the deadline a Ready lease can only be adopted.
ReclaimStaleLease(service) ==
    /\ service \in InitdManaged
    /\ recovery[service]
    /\ clock >= deadline[service]
    /\ state[service] \in {Admitted, Exited, Ready}
    /\ state' = [x \in Services |-> IF x \in Cascade(service) THEN Empty ELSE state[x]]
    /\ live' = [x \in Services |-> IF x \in Cascade(service) THEN FALSE ELSE live[x]]
    /\ endpoint' = [x \in Services |-> IF x \in Cascade(service) THEN FALSE ELSE endpoint[x]]
    /\ reporter' = [x \in Services |-> IF x \in Cascade(service) THEN "none" ELSE reporter[x]]
    /\ recovery' = [recovery EXCEPT ![service] = FALSE]
    /\ tracked' = [tracked EXCEPT ![service] = FALSE]
    /\ UNCHANGED <<currentInitd, deadline, clock>>

\* Orphaned (fenced) services keep running only while an adoption deadline is
\* armed; tracked ones are bound to the live initd.
FencedServiceHasDeadline ==
    \A service \in InitdManaged:
        (currentInitd = "new" /\ reporter[service] = "old" /\ state[service] # Empty
         /\ state[service] # Exited)
            => recovery[service]

TrackedServiceHasLiveReporter ==
    \A service \in InitdManaged:
        tracked[service] => reporter[service] = currentInitd

\* Supervisor death alone never kills or de-registers a service.
InitdExitPreservesServices ==
    [][ (currentInitd = "old" /\ currentInitd' = "new")
          => (live' = live /\ endpoint' = endpoint /\ state' = state) ]_vars

SettleRecovery(service) ==
    AdoptExactReadyLease(service) \/ ReclaimStaleLease(service)

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
    /\ deadline \in [Services -> 0..MaxTick]
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

RecoveryEventuallySettles ==
    \A service \in InitdManaged: recovery[service] ~> ~recovery[service]

Spec == Init /\ [][Next]_vars
        /\ SF_vars(AdvanceClock)
        /\ \A service \in InitdManaged: WF_vars(SettleRecovery(service))
================================================================================
