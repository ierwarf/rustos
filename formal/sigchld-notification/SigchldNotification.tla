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

baseVars == <<pending, noCldStop, selected, selectedNoCldStop, outcome,
          deliveredCauses, suppressedCauses>>

BaseInit ==
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

BaseNext ==
    \/ \E cause \in Causes: Queue(cause)
    \/ \E value \in BOOLEAN: SetNoCldStop(value)
    \/ Select
    \/ \E cause \in Causes: ConcurrentConsume(cause)
    \/ Commit
    \/ RejectStale

(***************************************************************************
Source-refining job-control component: process_placement::Change owns the
original process pin and frozen membership, Cohort owns both original UID
pins and dispatch admission. SetJobGate models each exact identity leaf;
terminal close may race but cannot reuse a pinned member or resurrect it.
RecordJobEvent feeds the SAME coalescing Queue above before one release.
Original policy actions and mutation obligations remain unchanged/unrestricted.
This is a bounded cold substrate, NOT physical MM/RSP or full dispatcher joint.
***************************************************************************)
JobMembers == {"leader", "worker"}
VARIABLES jobPhase, jobPins, jobRecipient, jobDesired, jobGates, jobUpdated,
          jobEvent, jobOpen, jobClosed, jobReused, jobChanged
jobVars == <<jobPhase, jobPins, jobRecipient, jobDesired, jobGates, jobUpdated,
             jobEvent, jobOpen, jobClosed, jobReused, jobChanged>>
vars == <<baseVars, jobVars>>
Init ==
    /\ BaseInit
    /\ jobPhase = "idle"
    /\ jobPins = {}
    /\ jobRecipient = FALSE
    /\ jobDesired = FALSE
    /\ jobGates = [m \in JobMembers |-> FALSE]
    /\ jobUpdated = {}
    /\ jobEvent = FALSE
    /\ jobOpen = TRUE
    /\ jobClosed = {}
    /\ jobReused = {}
    /\ jobChanged = FALSE
CaptureJob(desired) ==
    /\ jobPhase = "idle"
    /\ jobPhase' = "effects"
    /\ jobPins' = JobMembers
    /\ jobRecipient' = TRUE
    /\ jobDesired' = desired
    /\ jobOpen' = FALSE
    /\ UNCHANGED <<baseVars, jobGates, jobUpdated, jobEvent,
                    jobClosed, jobReused, jobChanged>>
SetJobGate(m) ==
    /\ jobPhase = "effects"
    /\ m \in JobMembers \ jobUpdated
    /\ jobPins = JobMembers
    /\ jobGates' = IF m \in jobClosed THEN jobGates
                    ELSE [jobGates EXCEPT ![m] = jobDesired]
    /\ jobUpdated' = jobUpdated \cup {m}
    /\ jobChanged' = (jobChanged \/ (m \notin jobClosed /\ jobGates[m] # jobDesired))
    /\ UNCHANGED <<baseVars, jobPhase, jobPins, jobRecipient, jobDesired,
                    jobEvent, jobOpen, jobClosed, jobReused>>
RecordJobEvent ==
    /\ jobPhase = "effects"
    /\ jobUpdated = JobMembers
    /\ jobPhase' = "event"
    /\ jobEvent' = TRUE
    /\ IF jobChanged THEN Queue(IF jobDesired THEN "stop" ELSE "continue")
                     ELSE UNCHANGED baseVars
    /\ UNCHANGED <<jobPins, jobRecipient, jobDesired, jobGates, jobUpdated,
                    jobOpen, jobClosed, jobReused, jobChanged>>
PublishJob ==
    /\ jobPhase = "event"
    /\ jobUpdated = JobMembers /\ jobEvent
    /\ jobPhase' = "published"
    /\ jobOpen' = TRUE
    /\ UNCHANGED <<baseVars, jobPins, jobRecipient, jobDesired, jobGates,
                    jobUpdated, jobEvent, jobClosed, jobReused, jobChanged>>
ReleaseJob ==
    /\ jobPhase = "published"
    /\ jobPhase' = "done"
    /\ jobPins' = {}
    /\ jobRecipient' = FALSE
    /\ UNCHANGED <<baseVars, jobDesired, jobGates, jobUpdated, jobEvent,
                    jobOpen, jobClosed, jobReused, jobChanged>>
CloseJobMember(m) ==
    /\ jobPhase \in {"effects", "event", "published", "done"}
    /\ m \in JobMembers \ jobClosed
    /\ jobClosed' = jobClosed \cup {m}
    /\ UNCHANGED <<baseVars, jobPhase, jobPins, jobRecipient, jobDesired,
                    jobGates, jobUpdated, jobEvent, jobOpen, jobReused, jobChanged>>
ReuseJobMember(m) ==
    /\ m \in jobClosed \ jobReused
    /\ m \notin jobPins
    /\ jobReused' = jobReused \cup {m}
    /\ UNCHANGED <<baseVars, jobPhase, jobPins, jobRecipient, jobDesired,
                    jobGates, jobUpdated, jobEvent, jobOpen, jobClosed, jobChanged>>
Next ==
    \/ /\ BaseNext /\ UNCHANGED jobVars
    \/ \E desired \in BOOLEAN: CaptureJob(desired)
    \/ \E m \in JobMembers: SetJobGate(m)
    \/ RecordJobEvent
    \/ PublishJob
    \/ ReleaseJob
    \/ \E m \in JobMembers: CloseJobMember(m)
    \/ \E m \in JobMembers: ReuseJobMember(m)

Spec == Init /\ [][Next]_vars
JobTypeOK ==
    /\ jobPhase \in {"idle", "effects", "event", "published", "done"}
    /\ jobPins \in SUBSET JobMembers /\ jobUpdated \in SUBSET JobMembers
    /\ jobClosed \in SUBSET JobMembers /\ jobReused \in SUBSET JobMembers
    /\ jobRecipient \in BOOLEAN /\ jobDesired \in BOOLEAN
    /\ jobEvent \in BOOLEAN /\ jobOpen \in BOOLEAN
    /\ jobChanged \in BOOLEAN
    /\ jobGates \in [JobMembers -> BOOLEAN]
OriginalJobUnitsThroughLastEffect ==
    jobPhase \in {"effects", "event", "published"} =>
        /\ jobPins = JobMembers /\ jobRecipient /\ jobReused = {}
NoPartialJobAdmission == jobPhase \in {"effects", "event"} => ~jobOpen
AllJobGatesBeforeRelease ==
    jobPhase \in {"published", "done"} =>
        /\ jobUpdated = JobMembers /\ jobEvent
        /\ \A m \in JobMembers \ jobClosed: jobGates[m] = jobDesired

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
