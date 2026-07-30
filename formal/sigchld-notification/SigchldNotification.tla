---------------------- MODULE SigchldNotification ----------------------
EXTENDS FiniteSets

(***************************************************************************
Owner: procd SIGCHLD disposition policy plus ring0 coalesced cause substrate.
Linearization point: ring0 removes only the cause snapshot revalidated after
procd returns. A cause queued after selection must remain pending.
***************************************************************************)

CONSTANTS Causes, ExitCause
NoOutcome == "none"
Delivered == "delivered"
Suppressed == "suppressed"
Rejected == "rejected"

VARIABLES pending, noCldStop, selected, selectedNoCldStop, outcome,
          deliveredCauses, suppressedCauses

vars == <<pending, noCldStop, selected, selectedNoCldStop, outcome,
          deliveredCauses, suppressedCauses>>

Init ==
    /\ pending = {}
    /\ noCldStop = FALSE
    /\ selected = {}
    /\ selectedNoCldStop = FALSE
    /\ outcome = NoOutcome
    /\ deliveredCauses = {}
    /\ suppressedCauses = {}

Queue(cause) ==
    /\ cause \in Causes
    /\ pending' = pending \cup {cause}
    /\ UNCHANGED <<noCldStop, selected, selectedNoCldStop, outcome,
                    deliveredCauses, suppressedCauses>>

SetNoCldStop(value) ==
    /\ value \in BOOLEAN
    /\ noCldStop' = value
    /\ UNCHANGED <<pending, selected, selectedNoCldStop, outcome,
                    deliveredCauses, suppressedCauses>>

Select ==
    /\ selected = {}
    /\ pending # {}
    /\ selected' = pending
    /\ selectedNoCldStop' = noCldStop
    /\ outcome' = NoOutcome
    /\ UNCHANGED <<pending, noCldStop, deliveredCauses, suppressedCauses>>

ConcurrentConsume(cause) ==
    /\ selected # {}
    /\ cause \in pending
    /\ pending' = pending \ {cause}
    /\ UNCHANGED <<noCldStop, selected, selectedNoCldStop, outcome,
                    deliveredCauses, suppressedCauses>>

Commit ==
    /\ selected # {}
    /\ selected \subseteq pending
    /\ pending' = pending \ selected
    /\ outcome' =
        IF selectedNoCldStop /\ ExitCause \notin selected
        THEN Suppressed ELSE Delivered
    /\ deliveredCauses' =
        IF selectedNoCldStop /\ ExitCause \notin selected
        THEN deliveredCauses ELSE deliveredCauses \cup selected
    /\ suppressedCauses' =
        IF selectedNoCldStop /\ ExitCause \notin selected
        THEN suppressedCauses \cup selected ELSE suppressedCauses
    /\ selected' = {}
    /\ UNCHANGED <<noCldStop, selectedNoCldStop>>

RejectStale ==
    /\ selected # {}
    /\ ~(selected \subseteq pending)
    /\ selected' = {}
    /\ outcome' = Rejected
    /\ UNCHANGED <<pending, noCldStop, selectedNoCldStop,
                    deliveredCauses, suppressedCauses>>

Next ==
    \/ \E cause \in Causes: Queue(cause)
    \/ \E value \in BOOLEAN: SetNoCldStop(value)
    \/ Select
    \/ \E cause \in Causes: ConcurrentConsume(cause)
    \/ Commit
    \/ RejectStale

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ ExitCause \in Causes
    /\ pending \in SUBSET Causes
    /\ noCldStop \in BOOLEAN
    /\ selected \in SUBSET Causes
    /\ selectedNoCldStop \in BOOLEAN
    /\ outcome \in {NoOutcome, Delivered, Suppressed, Rejected}
    /\ deliveredCauses \in SUBSET Causes
    /\ suppressedCauses \in SUBSET Causes

ExitIsNeverSuppressed == ExitCause \notin suppressedCauses
SuppressionContainsNoExit == suppressedCauses \cap {ExitCause} = {}
PendingCauseIsNotConsumedByOlderSnapshot ==
    selected \subseteq Causes /\ pending \in SUBSET Causes

=============================================================================
