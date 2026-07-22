----------------------- MODULE NetdDeferredReply -----------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Owner: netd local AF_UNIX deferred poll path.
Linearization point: global atomic reservation, retained while a worker has
detached a batch from the mutex queue, and released with the one terminal
reply. This models concurrent admission during detached batch processing.
***************************************************************************)

CONSTANTS Requests, MaxPending
Free == "free"
Queued == "queued"
Detached == "detached"
Replied == "replied"
Rejected == "rejected"
NoOutcome == "none"
Ready == "ready"
TimedOut == "timed-out"
QueueFailed == "queue-failed"
VARIABLES phase, reserved, replyAttempts, outcome
vars == <<phase, reserved, replyAttempts, outcome>>

Init ==
    /\ phase = [r \in Requests |-> Free]
    /\ reserved = {}
    /\ replyAttempts = [r \in Requests |-> 0]
    /\ outcome = [r \in Requests |-> NoOutcome]

Admit(r) ==
    /\ phase[r] = Free /\ Cardinality(reserved) < MaxPending
    /\ phase' = [phase EXCEPT ![r] = Queued]
    /\ reserved' = reserved \cup {r}
    /\ UNCHANGED <<replyAttempts, outcome>>

RejectFull(r) ==
    /\ phase[r] = Free /\ Cardinality(reserved) >= MaxPending
    /\ phase' = [phase EXCEPT ![r] = Rejected]
    /\ UNCHANGED <<reserved, replyAttempts, outcome>>

Detach(r) ==
    /\ phase[r] = Queued
    /\ phase' = [phase EXCEPT ![r] = Detached]
    /\ UNCHANGED <<reserved, replyAttempts, outcome>>

Requeue(r) ==
    /\ phase[r] = Detached
    /\ phase' = [phase EXCEPT ![r] = Queued]
    /\ UNCHANGED <<reserved, replyAttempts, outcome>>

Resolve(r, result) ==
    /\ phase[r] \in {Queued, Detached}
    /\ result \in {Ready, TimedOut, QueueFailed}
    /\ phase' = [phase EXCEPT ![r] = Replied]
    /\ reserved' = reserved \ {r}
    /\ replyAttempts' = [replyAttempts EXCEPT ![r] = @ + 1]
    /\ outcome' = [outcome EXCEPT ![r] = result]

ResolveAccepted(r) ==
    \E result \in {Ready, TimedOut, QueueFailed}: Resolve(r, result)

Next ==
    \/ \E r \in Requests: Admit(r) \/ RejectFull(r) \/ Detach(r)
                              \/ Requeue(r)
    \/ \E r \in Requests, result \in {Ready, TimedOut, QueueFailed}:
           Resolve(r, result)
AcceptedRequestEventuallyAttemptsReply ==
    \A r \in Requests:
        phase[r] \in {Queued, Detached} ~> phase[r] = Replied

Spec == Init /\ [][Next]_vars
        /\ \A r \in Requests: WF_vars(ResolveAccepted(r))

TypeOK ==
    /\ phase \in [Requests -> {Free, Queued, Detached, Replied, Rejected}]
    /\ reserved \in SUBSET Requests
    /\ replyAttempts \in [Requests -> 0..1]
    /\ outcome \in [Requests -> {NoOutcome, Ready, TimedOut, QueueFailed}]
GlobalBoundIncludesDetached == Cardinality(reserved) <= MaxPending
ReservationMatchesLiveRequest ==
    \A r \in Requests: (r \in reserved) <=> (phase[r] \in {Queued, Detached})
ExactlyOneTerminalReplyAttempt ==
    \A r \in Requests: (replyAttempts[r] = 1) <=>
        (phase[r] = Replied /\ outcome[r] # NoOutcome)
RejectedOwnsNoReplyAuthority ==
    \A r \in Requests: phase[r] = Rejected =>
        r \notin reserved /\ replyAttempts[r] = 0 /\ outcome[r] = NoOutcome
DetachedBatchRetainsCapacity ==
    \A r \in Requests: phase[r] = Detached => r \in reserved

=============================================================================
