------------------- MODULE PostInitBootstrapBarrier -------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Models initd's dependency-safe overlap of independent service activation.

Activation may overlap: a child with an exact rootd lease can initialize while
initd prepares another independent child. Activation alone is never dependency
authority. Each service must publish an endpoint owned by its exact admitted
PID, and initd must observe that binding before the consumer barrier opens.

Concrete owner:
  * services/initd/src/main.rs
***************************************************************************)

CONSTANT Services

Absent == "absent"
Active == "active"
Published == "published"
Admitted == "admitted"
Failed == "failed"

VARIABLES state, exactOwner, consumerStarted, rejectedForeign, panicked

vars == <<state, exactOwner, consumerStarted, rejectedForeign, panicked>>

Init ==
    /\ state = [service \in Services |-> Absent]
    /\ exactOwner = [service \in Services |-> FALSE]
    /\ consumerStarted = FALSE
    /\ rejectedForeign = FALSE
    /\ panicked = FALSE

Activate(service) ==
    /\ ~consumerStarted /\ ~panicked
    /\ state[service] = Absent
    /\ state' = [state EXCEPT ![service] = Active]
    /\ UNCHANGED <<exactOwner, consumerStarted, rejectedForeign, panicked>>

PublishExactEndpoint(service) ==
    /\ ~consumerStarted /\ ~panicked
    /\ state[service] = Active
    /\ state' = [state EXCEPT ![service] = Published]
    /\ exactOwner' = [exactOwner EXCEPT ![service] = TRUE]
    /\ UNCHANGED <<consumerStarted, rejectedForeign, panicked>>

AdmitExactEndpoint(service) ==
    /\ ~consumerStarted /\ ~panicked
    /\ state[service] = Published /\ exactOwner[service]
    /\ state' = [state EXCEPT ![service] = Admitted]
    /\ UNCHANGED <<exactOwner, consumerStarted, rejectedForeign, panicked>>

RejectForeignEndpoint(service) ==
    /\ ~consumerStarted /\ ~panicked
    /\ state[service] \in {Active, Published}
    /\ rejectedForeign' = TRUE
    /\ UNCHANGED <<state, exactOwner, consumerStarted, panicked>>

FailBeforeBarrier(service) ==
    /\ ~consumerStarted /\ ~panicked
    /\ state[service] \in {Active, Published, Admitted}
    /\ state' = [state EXCEPT ![service] = Failed]
    /\ exactOwner' = [exactOwner EXCEPT ![service] = FALSE]
    /\ UNCHANGED <<consumerStarted, rejectedForeign, panicked>>

StartConsumer ==
    /\ ~consumerStarted /\ ~panicked
    /\ \A service \in Services: state[service] = Admitted
    /\ consumerStarted' = TRUE
    /\ UNCHANGED <<state, exactOwner, rejectedForeign, panicked>>

AttemptEarlyConsumer ==
    /\ ~consumerStarted /\ ~panicked
    /\ \E service \in Services: state[service] # Admitted
    /\ panicked' = TRUE
    /\ UNCHANGED <<state, exactOwner, consumerStarted, rejectedForeign>>

TerminalStutter ==
    /\ (consumerStarted \/ panicked \/ \E service \in Services: state[service] = Failed)
    /\ UNCHANGED vars

Next ==
    \/ \E service \in Services:
        Activate(service)
        \/ PublishExactEndpoint(service)
        \/ AdmitExactEndpoint(service)
        \/ RejectForeignEndpoint(service)
        \/ FailBeforeBarrier(service)
    \/ StartConsumer
    \/ AttemptEarlyConsumer
    \/ TerminalStutter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ state \in [Services -> {Absent, Active, Published, Admitted, Failed}]
    /\ exactOwner \in [Services -> BOOLEAN]
    /\ consumerStarted \in BOOLEAN
    /\ rejectedForeign \in BOOLEAN
    /\ panicked \in BOOLEAN

AdmittedRequiresExactOwner ==
    \A service \in Services: state[service] = Admitted => exactOwner[service]

ConsumerRequiresCompleteBarrier ==
    consumerStarted => \A service \in Services: state[service] = Admitted

ActivationIsNotDependencyAuthority ==
    \A service \in Services: state[service] = Active => ~exactOwner[service]

FailureRevokesEndpointAuthority ==
    \A service \in Services: state[service] = Failed => ~exactOwner[service]

=============================================================================
