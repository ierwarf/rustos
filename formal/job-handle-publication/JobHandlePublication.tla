---------------- MODULE JobHandlePublication ----------------
EXTENDS Naturals

(***************************************************************************
One Job descriptor publication. The process-local slot is reserved before a
child is created (or a duplicate is prepared), copyout precedes publication,
and Drop cancels only the exact reservation and its unpublished empty child.
A revoked slot can be reserved again at a new epoch while copyout is pending.

Owner: job_ops.rs PendingJobHandle and HandleTable::commit_reserved_job.
Bounds: one pending operation, one replacement reservation, create/duplicate.
Job membership/generation and typed rights are separate registered models.
No claim about production user-copy faults, spinlocks or physical teardown.
***************************************************************************)

VARIABLES phase, copyOK, epoch, ticket, reserved, childOwed, createsChild,
          badPublication, badCancellation, revoked
vars == <<phase, copyOK, epoch, ticket, reserved, childOwed, createsChild,
          badPublication, badCancellation, revoked>>

Init ==
    /\ phase = "idle"
    /\ copyOK = FALSE
    /\ epoch = 1
    /\ ticket = 0
    /\ reserved = FALSE
    /\ childOwed = FALSE
    /\ createsChild \in BOOLEAN
    /\ badPublication = FALSE
    /\ badCancellation = FALSE
    /\ revoked = FALSE

Reserve ==
    /\ phase = "idle"
    /\ phase' = "reserved"
    /\ ticket' = epoch
    /\ reserved' = TRUE
    /\ childOwed' = createsChild
    /\ UNCHANGED <<copyOK, epoch, createsChild, badPublication, badCancellation, revoked>>

Copyout ==
    /\ phase = "reserved"
    /\ phase' = "copied"
    /\ copyOK' = TRUE
    /\ UNCHANGED <<epoch, ticket, reserved, childOwed, createsChild, badPublication, badCancellation, revoked>>

Revoke ==
    /\ phase \in {"reserved", "copied"}
    /\ reserved
    /\ reserved' = FALSE
    /\ revoked' = TRUE
    /\ UNCHANGED <<phase, copyOK, epoch, ticket, childOwed, createsChild, badPublication, badCancellation>>

Reuse ==
    /\ phase \in {"reserved", "copied"}
    /\ ~reserved /\ epoch = 1
    /\ epoch' = 2
    /\ reserved' = TRUE
    /\ UNCHANGED <<phase, copyOK, ticket, childOwed, createsChild, badPublication, badCancellation, revoked>>

\* Another publication skips this occupied slot and chooses a different fd.
\* Checking only the returned fd is insufficient: inserting then testing the
\* old map value would already have stolen this slot's reservation epoch.
ContendingReserve ==
    /\ phase \in {"reserved", "copied"}
    /\ reserved /\ ticket = epoch
    /\ epoch' = epoch \* preserve occupied reservation
    /\ UNCHANGED <<phase, copyOK, ticket, reserved, childOwed, createsChild,
                    badPublication, badCancellation, revoked>>

Publish ==
    /\ phase \in {"reserved", "copied"}
    /\ copyOK \* copyout guard
    /\ reserved
    /\ ticket = epoch \* exact reservation guard
    /\ phase' = "published"
    /\ badPublication' = (badPublication \/ ~copyOK \/ ~reserved \/ ticket # epoch)
    /\ reserved' = FALSE
    /\ childOwed' = FALSE
    /\ UNCHANGED <<copyOK, epoch, ticket, createsChild, badCancellation, revoked>>

Cancel ==
    /\ phase \in {"reserved", "copied"}
    /\ phase' = "cancelled"
    /\ reserved' = IF ticket = epoch THEN FALSE ELSE reserved
    /\ childOwed' = FALSE \* rollback child
    /\ badCancellation' = (badCancellation \/ (ticket # epoch /\ reserved /\ ~reserved'))
    /\ UNCHANGED <<copyOK, epoch, ticket, createsChild, badPublication, revoked>>

Next == Reserve \/ Copyout \/ Revoke \/ Reuse \/ ContendingReserve \/ Publish \/ Cancel \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {"idle", "reserved", "copied", "published", "cancelled"}
    /\ copyOK \in BOOLEAN /\ reserved \in BOOLEAN
    /\ childOwed \in BOOLEAN /\ createsChild \in BOOLEAN
    /\ epoch \in 1..2 /\ ticket \in 0..2
    /\ badPublication \in BOOLEAN /\ badCancellation \in BOOLEAN /\ revoked \in BOOLEAN
NoUncopiedOrStalePublication == ~badPublication
NoForeignReservationCancellation == ~badCancellation
LiveReservationIsNotStolen ==
    (phase \in {"reserved", "copied"} /\ ~revoked) => ticket = epoch
NoAbandonedChild == phase \in {"published", "cancelled"} => ~childOwed
=============================================================
