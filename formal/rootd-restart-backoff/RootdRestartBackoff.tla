-------------------------- MODULE RootdRestartBackoff --------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models rootd core-service restart backoff and authority lifecycle.

Concrete owners and source anchors:
  * restart state, budget, and policy-selected delay:
      services/rootd/src/main.rs (restart_failed_leases)
  * rootd-only bounded timer substrate:
      kernel/compat/src/user/syscall/linux/lifecycle_broker_ops.rs

The kernel never selects a retry delay or restarts a process. It merely offers
one rootd-capability-gated bounded wait. The model includes an observed exit,
the mandatory deferred-restart state, success/failure of a later launch, and
authority publication. It abstracts loader protocol bytes and scheduler time
granularity.
*******************************************************************************)

CONSTANTS Services, MaxRestarts, Backoff, MaxTick

Running == "running"
Exited == "exited"
Pending == "restart-pending"
Failed == "failed"
LeaseStates == {Running, Exited, Pending, Failed}

VARIABLES state, budget, published, retryAfter, clock, attempts

vars == <<state, budget, published, retryAfter, clock, attempts>>

Init ==
    /\ state = [service \in Services |-> Running]
    /\ budget = [service \in Services |-> MaxRestarts]
    /\ published = [service \in Services |-> TRUE]
    /\ retryAfter = [service \in Services |-> 0]
    /\ clock = 0
    /\ attempts = [service \in Services |-> 0]

\* Lifecycle exit first revokes the old PID's service authority. It cannot
\* directly launch the replacement in the same transition.
ObserveExit(service) ==
    /\ service \in Services
    /\ state[service] = Running
    /\ state' = [state EXCEPT ![service] = Exited]
    /\ published' = [published EXCEPT ![service] = FALSE]
    /\ UNCHANGED <<budget, retryAfter, clock, attempts>>

\* rootd converts an exit to a delayed retry before any spawn attempt. This
\* corresponds to the RESTART_PENDING branch and supervisor wait in Rust.
DeferExitedService(service) ==
    /\ service \in Services
    /\ state[service] = Exited
    /\ budget[service] > 0
    \* The finite TLC clock must include the complete backoff interval.
    /\ clock <= MaxTick - Backoff
    /\ state' = [state EXCEPT ![service] = Pending]
    /\ retryAfter' = [retryAfter EXCEPT ![service] = clock + Backoff]
    /\ UNCHANGED <<budget, published, clock, attempts>>

\* An exhausted lease becomes terminal and retains no old endpoint/capability.
ExhaustLease(service) ==
    /\ service \in Services
    /\ state[service] \in {Exited, Pending}
    /\ budget[service] = 0
    /\ state' = [state EXCEPT ![service] = Failed]
    /\ published' = [published EXCEPT ![service] = FALSE]
    /\ UNCHANGED <<budget, retryAfter, clock, attempts>>

\* The bounded rootd wait advances the only modeled time source.
AdvanceClock ==
    /\ clock < MaxTick
    \* Once a restart is due, time cannot silently move past it. TLC must
    \* explore success, a rescheduled failure, or exhausted-budget teardown.
    /\ \A service \in Services:
          state[service] = Pending => clock + 1 <= retryAfter[service]
    /\ clock' = clock + 1
    /\ UNCHANGED <<state, budget, published, retryAfter, attempts>>

\* A retry consumes exactly one budget unit and is unavailable before the
\* published deadline. A successful replacement gets fresh authority only at
\* this point.
RestartSucceeds(service) ==
    /\ service \in Services
    /\ state[service] = Pending
    /\ budget[service] > 0
    /\ clock >= retryAfter[service]
    /\ state' = [state EXCEPT ![service] = Running]
    /\ budget' = [budget EXCEPT ![service] = @ - 1]
    /\ published' = [published EXCEPT ![service] = TRUE]
    /\ attempts' = [attempts EXCEPT ![service] = @ + 1]
    /\ UNCHANGED <<retryAfter, clock>>

\* Spawn or activation failure consumes the same attempt and schedules another
\* bounded wait; it can never retain authority while pending.
RestartFails(service) ==
    /\ service \in Services
    /\ state[service] = Pending
    /\ budget[service] > 0
    /\ clock >= retryAfter[service]
    /\ clock <= MaxTick - Backoff
    /\ state' = state
    /\ budget' = [budget EXCEPT ![service] = @ - 1]
    /\ published' = [published EXCEPT ![service] = FALSE]
    /\ retryAfter' = [retryAfter EXCEPT ![service] = clock + Backoff]
    /\ attempts' = [attempts EXCEPT ![service] = @ + 1]
    /\ UNCHANGED clock

Next ==
    \/ \E service \in Services: ObserveExit(service)
    \/ \E service \in Services: DeferExitedService(service)
    \/ \E service \in Services: ExhaustLease(service)
    \/ AdvanceClock
    \/ \E service \in Services: RestartSucceeds(service)
    \/ \E service \in Services: RestartFails(service)

TypeOK ==
    /\ state \in [Services -> LeaseStates]
    /\ budget \in [Services -> 0..MaxRestarts]
    /\ published \in [Services -> BOOLEAN]
    /\ retryAfter \in [Services -> 0..MaxTick]
    /\ clock \in 0..MaxTick
    /\ attempts \in [Services -> 0..MaxRestarts]

OnlyRunningLeasePublishesAuthority ==
    \A service \in Services: published[service] => state[service] = Running

PendingOrTerminalLeaseHasNoAuthority ==
    \A service \in Services:
        state[service] \in {Exited, Pending, Failed} => ~published[service]

NoRetryAuthorityBeforePublishedDeadline ==
    \A service \in Services:
        state[service] = Pending /\ clock < retryAfter[service] => ~published[service]

PendingRestartDoesNotOutliveDeadline ==
    \A service \in Services:
        state[service] = Pending => clock <= retryAfter[service]

RestartBudgetIsFiniteAndMonotonic ==
    \A service \in Services: attempts[service] + budget[service] = MaxRestarts

Spec == Init /\ [][Next]_vars
================================================================================
