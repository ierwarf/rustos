-------------------- MODULE ApplicationLimitRefusal --------------------
EXTENDS Naturals

(***************************************************************************
Opt-in quota-proof protocol projection. Two workers stand for the native
workload's four, Limit=3 for its 32768-page ceiling. Exact page charging and
advisory admission slack are supplied by MemoryChargeConservation. Live
membership and unreaped process objects are DIFFERENT counters: Kill clears
live, Reap clears objects. A zero-live snapshot must not establish drain.
This checks proof eligibility and cleanup, not C/x86/transport execution.
***************************************************************************)
Limit == 3
Workers == 2
VARIABLES phase, enabled, limitMatch, headroom, protected, workers,
          faults, pages, live, objects, own, seen, seenPages, seenObjects,
          seenProtected, killed, killProtected
vars == <<phase, enabled, limitMatch, headroom, protected, workers,
          faults, pages, live, objects, own, seen, seenPages, seenObjects,
          seenProtected, killed, killProtected>>

Init ==
    /\ phase = "running"
    /\ enabled \in BOOLEAN /\ limitMatch \in BOOLEAN /\ headroom \in BOOLEAN
    /\ protected \in BOOLEAN
    /\ workers = Workers /\ faults = 0 /\ pages = 1 /\ live = 1 /\ objects \in 1..2
    /\ own = FALSE /\ seen = FALSE /\ seenPages = 0 /\ seenObjects = 0 /\ seenProtected = FALSE
    /\ killed = FALSE /\ killProtected = FALSE

Touch ==
    /\ phase = "running" /\ workers > 0 /\ pages < Limit
    /\ pages' = pages + 1
    /\ UNCHANGED <<phase, enabled, limitMatch, headroom, protected, workers,
        faults, live, objects, own, seen, seenPages, seenObjects, seenProtected, killed, killProtected>>

Fault ==
    /\ phase = "running" /\ workers > 0 /\ pages = Limit
    /\ workers' = workers - 1 /\ faults' = faults + 1
    /\ UNCHANGED <<phase, enabled, limitMatch, headroom, protected, pages,
        live, objects, own, seen, seenPages, seenObjects, seenProtected, killed, killProtected>>

\* Accounting alone cannot substitute for independently captured worker faults.
ExitWithoutFault ==
    /\ phase = "running" /\ workers > 0
    /\ workers' = workers - 1
    /\ UNCHANGED <<phase, enabled, limitMatch, headroom, protected, faults,
        pages, live, objects, own, seen, seenPages, seenObjects, seenProtected, killed, killProtected>>

ReapExtra ==
    /\ phase = "running" /\ objects > live
    /\ objects' = objects - 1
    /\ UNCHANGED <<phase, enabled, limitMatch, headroom, protected, workers,
        faults, pages, live, own, seen, seenPages, seenObjects, seenProtected, killed, killProtected>>

Observe ==
    /\ phase = "running" /\ workers = 0 /\ pages >= Limit - 1
    /\ live = 1 /\ objects = 1 \* one live member is not one process object
    /\ enabled /\ limitMatch /\ headroom \* explicit test mode and exact quota
    /\ ~protected
    /\ phase' = "observed" /\ own' = TRUE /\ seen' = TRUE
    /\ seenPages' = pages /\ seenObjects' = objects /\ seenProtected' = protected
    /\ UNCHANGED <<enabled, limitMatch, headroom, protected, workers,
        faults, pages, live, objects, killed, killProtected>>

Protect ==
    /\ phase = "observed" /\ ~protected
    /\ protected' = TRUE
    /\ UNCHANGED <<phase, enabled, limitMatch, headroom, workers, faults,
        pages, live, objects, own, seen, seenPages, seenObjects, seenProtected, killed, killProtected>>

Kill ==
    /\ phase = "observed"
    /\ ~protected \* recheck immediately before cleanup
    /\ phase' = "draining" /\ killed' = TRUE /\ killProtected' = protected /\ live' = 0
    /\ UNCHANGED <<enabled, limitMatch, headroom, protected, workers, faults,
        pages, objects, own, seen, seenPages, seenObjects, seenProtected>>

Reject ==
    /\ phase = "observed" /\ protected
    /\ phase' = "rejected" /\ own' = own \* denied cleanup retains custody
    /\ UNCHANGED <<enabled, limitMatch, headroom, protected, workers, faults,
        pages, live, objects, seen, seenPages, seenObjects, seenProtected, killed, killProtected>>

Reap ==
    /\ phase = "draining" /\ objects > 0
    /\ objects' = objects - 1
    /\ UNCHANGED <<phase, enabled, limitMatch, headroom, protected, workers,
        faults, pages, live, own, seen, seenPages, seenObjects, seenProtected, killed, killProtected>>

Uncharge ==
    /\ phase = "draining" /\ pages > 0
    /\ pages' = pages - 1
    /\ UNCHANGED <<phase, enabled, limitMatch, headroom, protected, workers,
        faults, live, objects, own, seen, seenPages, seenObjects, seenProtected, killed, killProtected>>

Drained ==
    /\ phase = "draining" /\ live = 0
    /\ objects = 0 \* live zero is not object zero
    /\ pages = 0 \* a dead process is not yet zero-charge evidence
    /\ phase' = "done" /\ own' = FALSE
    /\ UNCHANGED <<enabled, limitMatch, headroom, protected, workers, faults,
        pages, live, objects, seen, seenPages, seenObjects, seenProtected, killed, killProtected>>

Accept ==
    /\ phase = "done"
    /\ faults = Workers \* four independently verified native worker records
    /\ phase' = "accepted"
    /\ UNCHANGED <<enabled, limitMatch, headroom, protected, workers, faults,
        pages, live, objects, own, seen, seenPages, seenObjects, seenProtected, killed, killProtected>>

Next == Touch \/ Fault \/ ExitWithoutFault \/ ReapExtra \/ Observe \/ Protect \/ Kill
    \/ Reject \/ Reap \/ Uncharge \/ Drained \/ Accept \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {"running", "observed", "draining", "rejected", "done", "accepted"}
    /\ enabled \in BOOLEAN /\ limitMatch \in BOOLEAN /\ headroom \in BOOLEAN
    /\ protected \in BOOLEAN /\ workers \in 0..Workers /\ faults \in 0..Workers
    /\ pages \in 0..Limit /\ live \in 0..1 /\ objects \in 0..2 /\ live <= objects /\ own \in BOOLEAN
    /\ seen \in BOOLEAN /\ seenPages \in 0..Limit /\ seenObjects \in 0..2 /\ seenProtected \in BOOLEAN
    /\ killed \in BOOLEAN /\ killProtected \in BOOLEAN
ObservedHasExactQuota == seen => enabled /\ limitMatch /\ headroom /\ ~seenProtected /\ seenPages >= Limit - 1 /\ seenObjects = 1
CleanupHasSafeScope == killed => ~killProtected
RetirementRetainsCustody == phase \in {"observed", "draining", "rejected"} => own
DrainedMeansNoObjectsOrPages == phase \in {"done", "accepted"} => live = 0 /\ objects = 0 /\ pages = 0
AcceptedRequiresAllFaults == phase = "accepted" => faults = Workers
=============================================================================
