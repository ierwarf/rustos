----------------------- MODULE NetdDeferredReply -----------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Owner: netd local AF_UNIX deferred poll path.

The caller supplies one immutable absolute wire end (`WireEnd`).  Admission is
the only transition which derives service authority: it stamps
`min(WireEnd, admission time + ClassCap)`.  Queueing, detaching, and requeueing
only carry that stamp.  An already-expired request is rejected before it can
reserve a deferred slot; an admitted request which expires is terminal, makes
one reply attempt, and releases its reservation in the same tick.

Linearization point: global atomic reservation, retained while a worker has
detached a batch from the mutex queue, and released with the one terminal
reply.  This models concurrent admission during detached batch processing.
***************************************************************************)

CONSTANTS Requests, MaxPending, WireEnd, ClassCap, MaxTime

Free == "free"
Queued == "queued"
Detached == "detached"
Replied == "replied"
Rejected == "rejected"

NoOutcome == "none"
Ready == "ready"
TimedOut == "timed-out"
QueueFailed == "queue-failed"

NoRejection == "none"
ExpiredAtAdmission == "expired-at-admission"
QueueFull == "queue-full"

NoTime == MaxTime + 1
NoEnd == 0

VARIABLES now,
          phase,
          reserved,
          replyAttempts,
          outcome,
          admittedAt,
          effectiveEnd,
          rejection

vars == <<now, phase, reserved, replyAttempts, outcome, admittedAt,
          effectiveEnd, rejection>>

Live(r) == phase[r] \in {Queued, Detached}
StampedEndAt(admissionTime) ==
    IF WireEnd < admissionTime + ClassCap
    THEN WireEnd
    ELSE admissionTime + ClassCap
ExpiresOnTick(r) == Live(r) /\ effectiveEnd[r] <= now + 1

Init ==
    /\ now = 0
    /\ phase = [r \in Requests |-> Free]
    /\ reserved = {}
    /\ replyAttempts = [r \in Requests |-> 0]
    /\ outcome = [r \in Requests |-> NoOutcome]
    /\ admittedAt = [r \in Requests |-> NoTime]
    /\ effectiveEnd = [r \in Requests |-> NoEnd]
    /\ rejection = [r \in Requests |-> NoRejection]

Admit(r) ==
    /\ phase[r] = Free
    /\ now < WireEnd
    /\ Cardinality(reserved) < MaxPending
    /\ phase' = [phase EXCEPT ![r] = Queued]
    /\ reserved' = reserved \cup {r}
    /\ admittedAt' = [admittedAt EXCEPT ![r] = now]
    /\ effectiveEnd' = [effectiveEnd EXCEPT ![r] = StampedEndAt(now)]
    /\ UNCHANGED <<now, replyAttempts, outcome, rejection>>

RejectExpired(r) ==
    /\ phase[r] = Free
    /\ now >= WireEnd
    /\ phase' = [phase EXCEPT ![r] = Rejected]
    /\ rejection' = [rejection EXCEPT ![r] = ExpiredAtAdmission]
    /\ UNCHANGED <<now, reserved, replyAttempts, outcome, admittedAt,
                   effectiveEnd>>

RejectFull(r) ==
    /\ phase[r] = Free
    /\ now < WireEnd
    /\ Cardinality(reserved) >= MaxPending
    /\ phase' = [phase EXCEPT ![r] = Rejected]
    /\ rejection' = [rejection EXCEPT ![r] = QueueFull]
    /\ UNCHANGED <<now, reserved, replyAttempts, outcome, admittedAt,
                   effectiveEnd>>

Detach(r) ==
    /\ phase[r] = Queued
    /\ phase' = [phase EXCEPT ![r] = Detached]
    /\ UNCHANGED <<now, reserved, replyAttempts, outcome, admittedAt,
                   effectiveEnd, rejection>>

Requeue(r) ==
    /\ phase[r] = Detached
    /\ phase' = [phase EXCEPT ![r] = Queued]
    /\ UNCHANGED <<now, reserved, replyAttempts, outcome, admittedAt,
                   effectiveEnd, rejection>>

Resolve(r, result) ==
    /\ Live(r)
    /\ now < effectiveEnd[r]
    /\ result \in {Ready, QueueFailed}
    /\ phase' = [phase EXCEPT ![r] = Replied]
    /\ reserved' = reserved \ {r}
    /\ replyAttempts' = [replyAttempts EXCEPT ![r] = @ + 1]
    /\ outcome' = [outcome EXCEPT ![r] = result]
    /\ UNCHANGED <<now, admittedAt, effectiveEnd, rejection>>

ResolveAccepted(r) ==
    \E result \in {Ready, QueueFailed}: Resolve(r, result)

Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ phase' =
        [r \in Requests |-> IF ExpiresOnTick(r) THEN Replied ELSE phase[r]]
    /\ reserved' = {r \in reserved : ~ExpiresOnTick(r)}
    /\ replyAttempts' =
        [r \in Requests |->
            IF ExpiresOnTick(r) THEN replyAttempts[r] + 1 ELSE replyAttempts[r]]
    /\ outcome' =
        [r \in Requests |->
            IF ExpiresOnTick(r) THEN TimedOut ELSE outcome[r]]
    /\ UNCHANGED <<admittedAt, effectiveEnd, rejection>>

Next ==
    \/ \E r \in Requests:
        Admit(r) \/ RejectExpired(r) \/ RejectFull(r) \/ Detach(r) \/ Requeue(r)
    \/ \E r \in Requests, result \in {Ready, QueueFailed}: Resolve(r, result)
    \/ Tick

AcceptedRequestEventuallyAttemptsReply ==
    \A r \in Requests:
        Live(r) ~> phase[r] = Replied

Spec == Init /\ [][Next]_vars
        /\ \A r \in Requests: WF_vars(ResolveAccepted(r))

TypeOK ==
    /\ Requests # {}
    /\ MaxPending \in 1..Cardinality(Requests)
    /\ WireEnd \in 1..MaxTime
    /\ ClassCap \in Nat \ {0}
    /\ now \in 0..MaxTime
    /\ phase \in [Requests -> {Free, Queued, Detached, Replied, Rejected}]
    /\ reserved \in SUBSET Requests
    /\ replyAttempts \in [Requests -> 0..1]
    /\ outcome \in [Requests -> {NoOutcome, Ready, TimedOut, QueueFailed}]
    /\ admittedAt \in [Requests -> 0..NoTime]
    /\ effectiveEnd \in [Requests -> 0..MaxTime]
    /\ rejection \in [Requests -> {NoRejection, ExpiredAtAdmission, QueueFull}]

GlobalBoundIncludesDetached == Cardinality(reserved) <= MaxPending

ReservationMatchesLiveDeferredRequest ==
    \A r \in Requests: (r \in reserved) <=> Live(r)

AdmittedBeforeWireDeadline ==
    \A r \in Requests:
        phase[r] \in {Queued, Detached, Replied} => admittedAt[r] < WireEnd

StampedEndMatchesAdmissionClamp ==
    \A r \in Requests:
        phase[r] \in {Queued, Detached, Replied} =>
            /\ effectiveEnd[r] = StampedEndAt(admittedAt[r])
            /\ effectiveEnd[r] <= WireEnd
            /\ effectiveEnd[r] <= admittedAt[r] + ClassCap

LiveDeferredRequestHasFutureStampedEnd ==
    \A r \in Requests: Live(r) => now < effectiveEnd[r]

ExactlyOneTerminalReplyAttempt ==
    \A r \in Requests: (replyAttempts[r] = 1) <=>
        (phase[r] = Replied /\ outcome[r] # NoOutcome)

ExpiredDeferredRequestIsTerminal ==
    \A r \in Requests:
        phase[r] = Replied /\ outcome[r] = TimedOut =>
            /\ replyAttempts[r] = 1
            /\ r \notin reserved
            /\ admittedAt[r] < WireEnd
            /\ effectiveEnd[r] <= now

RejectedBeforeAdmissionOwnsNoDeferredReservation ==
    \A r \in Requests:
        rejection[r] # NoRejection =>
            /\ phase[r] = Rejected
            /\ r \notin reserved
            /\ replyAttempts[r] = 0
            /\ outcome[r] = NoOutcome
            /\ admittedAt[r] = NoTime
            /\ effectiveEnd[r] = NoEnd

NonAdmittedRequestsHaveNoDeadlineStamp ==
    \A r \in Requests:
        phase[r] \in {Free, Rejected} =>
            /\ admittedAt[r] = NoTime
            /\ effectiveEnd[r] = NoEnd

=============================================================================
