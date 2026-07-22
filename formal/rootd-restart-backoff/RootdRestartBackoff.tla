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
authority publication. The lifecycle evidence slice additionally models the
bounded append-only exit queue, exclusive drain snapshot, partial copyout,
commit, and sticky overflow paths. Rootd and procd receive independent fan-out
queues: draining process-policy evidence cannot consume supervisor evidence,
and process-policy overflow clears its cached authority before rebasing. It
abstracts loader protocol bytes, individual queue order, and scheduler time
granularity.
*******************************************************************************)

CONSTANTS Services, MaxRestarts, Backoff, MaxTick, LifecycleEvents

Running == "running"
Exited == "exited"
Pending == "restart-pending"
Failed == "failed"
LeaseStates == {Running, Exited, Pending, Failed}

DrainIdle == "idle"
DrainSnapshotted == "snapshotted"
DrainEventsCopied == "events-copied"
DrainCountCopied == "count-copied"
DrainStates == {DrainIdle, DrainSnapshotted, DrainEventsCopied, DrainCountCopied}

VARIABLES state, budget, published, retryAfter, clock, attempts,
          failedActivationChild, supervisorHealthy, lifecycleEvidenceComplete,
          lifecycleQueue, drainSnapshot, drainStage, policyLifecycleQueue,
          policyCacheAuthoritative, recordedLifecycleEvidence,
          rootConsumedEvidence

vars == <<state, budget, published, retryAfter, clock, attempts,
          failedActivationChild, supervisorHealthy, lifecycleEvidenceComplete,
          lifecycleQueue, drainSnapshot, drainStage, policyLifecycleQueue,
          policyCacheAuthoritative, recordedLifecycleEvidence,
          rootConsumedEvidence>>

policyEvidenceVars == <<policyLifecycleQueue, policyCacheAuthoritative>>
rootEvidenceHistoryVars == <<recordedLifecycleEvidence, rootConsumedEvidence>>

Init ==
    /\ state = [service \in Services |-> Running]
    /\ budget = [service \in Services |-> MaxRestarts]
    /\ published = [service \in Services |-> TRUE]
    /\ retryAfter = [service \in Services |-> 0]
    /\ clock = 0
    /\ attempts = [service \in Services |-> 0]
    /\ failedActivationChild = {}
    /\ supervisorHealthy = TRUE
    /\ lifecycleEvidenceComplete = TRUE
    /\ lifecycleQueue = {}
    /\ drainSnapshot = {}
    /\ drainStage = DrainIdle
    /\ policyLifecycleQueue = {}
    /\ policyCacheAuthoritative = TRUE
    /\ recordedLifecycleEvidence = {}
    /\ rootConsumedEvidence = {}

\* Lifecycle exit first revokes the old PID's service authority. It cannot
\* directly launch the replacement in the same transition.
ObserveExit(service) ==
    /\ supervisorHealthy
    /\ service \in Services
    /\ state[service] = Running
    /\ state' = [state EXCEPT ![service] = Exited]
    /\ published' = [published EXCEPT ![service] = FALSE]
    /\ UNCHANGED <<budget, retryAfter, clock, attempts,
                  failedActivationChild, supervisorHealthy, lifecycleEvidenceComplete,
                  lifecycleQueue, drainSnapshot, drainStage>>
    /\ UNCHANGED policyEvidenceVars
    /\ UNCHANGED rootEvidenceHistoryVars

\* rootd converts an exit to a delayed retry before any spawn attempt. This
\* corresponds to the RESTART_PENDING branch and supervisor wait in Rust.
DeferExitedService(service) ==
    /\ supervisorHealthy
    /\ service \in Services
    /\ state[service] = Exited
    /\ budget[service] > 0
    \* The finite TLC clock must include the complete backoff interval.
    /\ clock <= MaxTick - Backoff
    /\ state' = [state EXCEPT ![service] = Pending]
    /\ retryAfter' = [retryAfter EXCEPT ![service] = clock + Backoff]
    /\ UNCHANGED <<budget, published, clock, attempts,
                  failedActivationChild, supervisorHealthy, lifecycleEvidenceComplete,
                  lifecycleQueue, drainSnapshot, drainStage>>
    /\ UNCHANGED policyEvidenceVars
    /\ UNCHANGED rootEvidenceHistoryVars

\* An exhausted lease becomes terminal and retains no old endpoint/capability.
ExhaustLease(service) ==
    /\ supervisorHealthy
    /\ service \in Services
    /\ state[service] \in {Exited, Pending}
    /\ budget[service] = 0
    /\ state' = [state EXCEPT ![service] = Failed]
    /\ published' = [published EXCEPT ![service] = FALSE]
    /\ UNCHANGED <<budget, retryAfter, clock, attempts,
                  failedActivationChild, supervisorHealthy, lifecycleEvidenceComplete,
                  lifecycleQueue, drainSnapshot, drainStage>>
    /\ UNCHANGED policyEvidenceVars
    /\ UNCHANGED rootEvidenceHistoryVars

\* The bounded rootd wait advances the only modeled time source.
AdvanceClock ==
    /\ supervisorHealthy
    /\ clock < MaxTick
    \* Once a restart is due, time cannot silently move past it. TLC must
    \* explore success, a rescheduled failure, or exhausted-budget teardown.
    /\ \A service \in Services:
          state[service] = Pending => clock + 1 <= retryAfter[service]
    /\ clock' = clock + 1
    /\ UNCHANGED <<state, budget, published, retryAfter, attempts,
                  failedActivationChild, supervisorHealthy, lifecycleEvidenceComplete,
                  lifecycleQueue, drainSnapshot, drainStage>>
    /\ UNCHANGED policyEvidenceVars
    /\ UNCHANGED rootEvidenceHistoryVars

\* A retry consumes exactly one budget unit and is unavailable before the
\* published deadline. A successful replacement gets fresh authority only at
\* this point.
RestartSucceeds(service) ==
    /\ supervisorHealthy
    /\ service \in Services
    /\ state[service] = Pending
    /\ budget[service] > 0
    /\ clock >= retryAfter[service]
    /\ service \notin failedActivationChild
    /\ state' = [state EXCEPT ![service] = Running]
    /\ budget' = [budget EXCEPT ![service] = @ - 1]
    /\ published' = [published EXCEPT ![service] = TRUE]
    /\ attempts' = [attempts EXCEPT ![service] = @ + 1]
    /\ UNCHANGED <<retryAfter, clock, failedActivationChild, supervisorHealthy,
                  lifecycleEvidenceComplete, lifecycleQueue, drainSnapshot, drainStage>>
    /\ UNCHANGED policyEvidenceVars
    /\ UNCHANGED rootEvidenceHistoryVars

\* Spawn or activation failure consumes the same attempt and schedules another
\* bounded wait; it can never retain authority while pending.
RestartFails(service) ==
    /\ supervisorHealthy
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
    /\ UNCHANGED <<clock, failedActivationChild, supervisorHealthy,
                  lifecycleEvidenceComplete, lifecycleQueue, drainSnapshot, drainStage>>
    /\ UNCHANGED policyEvidenceVars
    /\ UNCHANGED rootEvidenceHistoryVars

\* A deferred spawn can succeed while ACTIVATE fails. Continuing recovery is
\* safe only after rootd's terminate broker retires that exact suspended PID.
ActivationFailsAndCleans(service) ==
    /\ supervisorHealthy
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
    /\ failedActivationChild' = failedActivationChild \ {service}
    /\ UNCHANGED <<clock, supervisorHealthy, lifecycleEvidenceComplete,
                  lifecycleQueue, drainSnapshot, drainStage>>
    /\ UNCHANGED policyEvidenceVars
    /\ UNCHANGED rootEvidenceHistoryVars

\* If exact-PID retirement is rejected, source fail-closes rootd instead of
\* retrying around a child whose lifecycle is uncertain.
ActivationCleanupFails(service) ==
    /\ supervisorHealthy
    /\ service \in Services
    /\ state[service] = Pending
    /\ budget[service] > 0
    /\ clock >= retryAfter[service]
    /\ state' = [state EXCEPT ![service] = Failed]
    /\ budget' = [budget EXCEPT ![service] = @ - 1]
    /\ published' = [published EXCEPT ![service] = FALSE]
    /\ attempts' = [attempts EXCEPT ![service] = @ + 1]
    /\ failedActivationChild' = failedActivationChild \cup {service}
    /\ supervisorHealthy' = FALSE
    /\ UNCHANGED <<retryAfter, clock, lifecycleEvidenceComplete,
                  lifecycleQueue, drainSnapshot, drainStage>>
    /\ UNCHANGED policyEvidenceVars
    /\ UNCHANGED rootEvidenceHistoryVars

SettlePending(service) ==
    RestartSucceeds(service)
    \/ RestartFails(service)
    \/ ActivationFailsAndCleans(service)
    \/ ActivationCleanupFails(service)
    \/ ExhaustLease(service)

\* Producers only append under the bounded queue lock. A drain snapshots the
\* exact current prefix, releases that lock for both user-memory writes, and
\* retires only the snapshot after the event array and count both commit.
RecordLifecycleEvidence(event) ==
    /\ supervisorHealthy
    /\ lifecycleEvidenceComplete
    /\ event \in LifecycleEvents \ recordedLifecycleEvidence
    /\ lifecycleQueue' = lifecycleQueue \cup {event}
    /\ policyLifecycleQueue' = policyLifecycleQueue \cup {event}
    /\ recordedLifecycleEvidence' = recordedLifecycleEvidence \cup {event}
    /\ UNCHANGED <<state, budget, published, retryAfter, clock, attempts,
                  failedActivationChild, supervisorHealthy, lifecycleEvidenceComplete,
                  drainSnapshot, drainStage, policyCacheAuthoritative,
                  rootConsumedEvidence>>

BeginLifecycleDrain ==
    /\ supervisorHealthy
    /\ lifecycleEvidenceComplete
    /\ drainStage = DrainIdle
    /\ lifecycleQueue # {}
    /\ drainSnapshot' = lifecycleQueue
    /\ drainStage' = DrainSnapshotted
    /\ UNCHANGED <<state, budget, published, retryAfter, clock, attempts,
                  failedActivationChild, supervisorHealthy, lifecycleEvidenceComplete,
                  lifecycleQueue>>
    /\ UNCHANGED policyEvidenceVars
    /\ UNCHANGED rootEvidenceHistoryVars

CopyLifecycleEvents ==
    /\ drainStage = DrainSnapshotted
    /\ drainStage' = DrainEventsCopied
    /\ UNCHANGED <<state, budget, published, retryAfter, clock, attempts,
                  failedActivationChild, supervisorHealthy, lifecycleEvidenceComplete,
                  lifecycleQueue, drainSnapshot>>
    /\ UNCHANGED policyEvidenceVars
    /\ UNCHANGED rootEvidenceHistoryVars

CopyLifecycleCount ==
    /\ drainStage = DrainEventsCopied
    /\ drainStage' = DrainCountCopied
    /\ UNCHANGED <<state, budget, published, retryAfter, clock, attempts,
                  failedActivationChild, supervisorHealthy, lifecycleEvidenceComplete,
                  lifecycleQueue, drainSnapshot>>
    /\ UNCHANGED policyEvidenceVars
    /\ UNCHANGED rootEvidenceHistoryVars

LifecycleCopyoutFails ==
    /\ drainStage \in {DrainSnapshotted, DrainEventsCopied}
    /\ drainSnapshot' = {}
    /\ drainStage' = DrainIdle
    /\ supervisorHealthy' = FALSE
    /\ UNCHANGED <<state, budget, published, retryAfter, clock, attempts,
                  failedActivationChild, lifecycleEvidenceComplete,
                  lifecycleQueue>>
    /\ UNCHANGED policyEvidenceVars
    /\ UNCHANGED rootEvidenceHistoryVars

CommitLifecycleDrain ==
    /\ drainStage = DrainCountCopied
    /\ lifecycleQueue' = lifecycleQueue \ drainSnapshot
    /\ rootConsumedEvidence' = rootConsumedEvidence \cup drainSnapshot
    /\ drainSnapshot' = {}
    /\ drainStage' = DrainIdle
    /\ UNCHANGED <<state, budget, published, retryAfter, clock, attempts,
                  failedActivationChild, supervisorHealthy, lifecycleEvidenceComplete,
                  recordedLifecycleEvidence>>
    /\ UNCHANGED policyEvidenceVars

SettleLifecycleDrain ==
    CopyLifecycleEvents
    \/ CopyLifecycleCount
    \/ LifecycleCopyoutFails
    \/ CommitLifecycleDrain

LifecycleEvidenceOverflows ==
    /\ supervisorHealthy
    /\ lifecycleEvidenceComplete
    /\ lifecycleQueue = LifecycleEvents
    /\ lifecycleEvidenceComplete' = FALSE
    /\ supervisorHealthy' = FALSE
    /\ UNCHANGED <<state, budget, published, retryAfter, clock, attempts,
                  failedActivationChild, lifecycleQueue, drainSnapshot, drainStage>>
    /\ UNCHANGED policyEvidenceVars
    /\ UNCHANGED rootEvidenceHistoryVars

\* procd owns a separate fan-out queue. Its successful drain cannot consume
\* rootd evidence. If only this queue overflows, procd clears every cached
\* process/thread policy and rebases this queue without weakening rootd.
CommitPolicyLifecycleDrain ==
    /\ policyLifecycleQueue # {}
    /\ policyLifecycleQueue' = {}
    /\ UNCHANGED <<state, budget, published, retryAfter, clock, attempts,
                  failedActivationChild, supervisorHealthy, lifecycleEvidenceComplete,
                  lifecycleQueue, drainSnapshot, drainStage, policyCacheAuthoritative>>
    /\ UNCHANGED rootEvidenceHistoryVars

PolicyLifecycleOverflowRebases ==
    /\ policyLifecycleQueue = LifecycleEvents
    /\ policyLifecycleQueue' = {}
    /\ policyCacheAuthoritative' = FALSE
    /\ UNCHANGED <<state, budget, published, retryAfter, clock, attempts,
                  failedActivationChild, supervisorHealthy, lifecycleEvidenceComplete,
                  lifecycleQueue, drainSnapshot, drainStage>>
    /\ UNCHANGED rootEvidenceHistoryVars

Next ==
    \/ \E service \in Services: ObserveExit(service)
    \/ \E service \in Services: DeferExitedService(service)
    \/ \E service \in Services: ExhaustLease(service)
    \/ AdvanceClock
    \/ \E service \in Services: RestartSucceeds(service)
    \/ \E service \in Services: RestartFails(service)
    \/ \E service \in Services: ActivationFailsAndCleans(service)
    \/ \E service \in Services: ActivationCleanupFails(service)
    \/ \E event \in LifecycleEvents: RecordLifecycleEvidence(event)
    \/ BeginLifecycleDrain
    \/ CopyLifecycleEvents
    \/ CopyLifecycleCount
    \/ LifecycleCopyoutFails
    \/ CommitLifecycleDrain
    \/ LifecycleEvidenceOverflows
    \/ CommitPolicyLifecycleDrain
    \/ PolicyLifecycleOverflowRebases

TypeOK ==
    /\ state \in [Services -> LeaseStates]
    /\ budget \in [Services -> 0..MaxRestarts]
    /\ published \in [Services -> BOOLEAN]
    /\ retryAfter \in [Services -> 0..MaxTick]
    /\ clock \in 0..MaxTick
    /\ attempts \in [Services -> 0..MaxRestarts]
    /\ failedActivationChild \subseteq Services
    /\ supervisorHealthy \in BOOLEAN
    /\ lifecycleEvidenceComplete \in BOOLEAN
    /\ lifecycleQueue \subseteq LifecycleEvents
    /\ drainSnapshot \subseteq LifecycleEvents
    /\ drainStage \in DrainStates
    /\ policyLifecycleQueue \subseteq LifecycleEvents
    /\ policyCacheAuthoritative \in BOOLEAN
    /\ recordedLifecycleEvidence \subseteq LifecycleEvents
    /\ rootConsumedEvidence \subseteq LifecycleEvents

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

HealthySupervisorHasNoFailedActivationChild ==
    supervisorHealthy => failedActivationChild = {}

FailedActivationChildCannotPublish ==
    \A service \in failedActivationChild: ~published[service]

IncompleteLifecycleEvidenceStopsSupervisor ==
    ~lifecycleEvidenceComplete => ~supervisorHealthy

PartialLifecycleCopyoutRetainsEvidence ==
    drainStage # DrainIdle => drainSnapshot \subseteq lifecycleQueue

LifecycleDrainIdentityIsExact ==
    (drainStage = DrainIdle) <=> (drainSnapshot = {})

RootLifecycleEvidenceIsNeverLost ==
    recordedLifecycleEvidence = lifecycleQueue \cup rootConsumedEvidence

PolicyRebaseCannotConsumeRootEvidence ==
    ~policyCacheAuthoritative =>
        recordedLifecycleEvidence = lifecycleQueue \cup rootConsumedEvidence

PendingRestartEventuallySettles ==
    \A service \in Services:
        state[service] = Pending ~> (state[service] # Pending \/ ~supervisorHealthy)

LifecycleWorkEventuallySettlesOrFailsClosed ==
    (drainStage # DrainIdle \/ lifecycleQueue # {}) ~>
        (drainStage = DrainIdle /\ (lifecycleQueue = {} \/ ~supervisorHealthy))

Spec == Init /\ [][Next]_vars
        /\ SF_vars(AdvanceClock)
        /\ \A service \in Services: WF_vars(SettlePending(service))
        /\ WF_vars(BeginLifecycleDrain)
        /\ WF_vars(SettleLifecycleDrain)
================================================================================
