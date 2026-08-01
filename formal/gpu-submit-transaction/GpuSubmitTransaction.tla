---------------------- MODULE GpuSubmitTransaction ----------------------
EXTENDS Naturals

(***************************************************************************
The compiler timeline and damage history commit only after the display
transport accepts a submit.  Rejection restores the exact bounded checkpoint
and requires a complete atlas replay on the next attempt.
***************************************************************************)

Idle == "idle"
Checkpointed == "checkpointed"
Compiled == "compiled"
Accepted == "accepted"
Rejected == "rejected"

VARIABLES phase, nextSubmit, checkpoint, timelineAdmitted, forceFullReplay,
          submittedFullSnapshot, damageCommitted, transportAccepted

vars == <<phase, nextSubmit, checkpoint, timelineAdmitted, forceFullReplay,
          submittedFullSnapshot, damageCommitted, transportAccepted>>

Init ==
    /\ phase = Idle /\ nextSubmit = 1 /\ checkpoint = 1
    /\ timelineAdmitted = FALSE /\ forceFullReplay = FALSE
    /\ submittedFullSnapshot = FALSE /\ damageCommitted = FALSE
    /\ transportAccepted = FALSE

TakeCheckpoint ==
    /\ phase \in {Idle, Rejected}
    /\ phase' = Checkpointed /\ checkpoint' = nextSubmit
    /\ UNCHANGED <<nextSubmit, timelineAdmitted, forceFullReplay,
                    submittedFullSnapshot, damageCommitted, transportAccepted>>

Compile ==
    /\ phase = Checkpointed
    /\ phase' = Compiled /\ nextSubmit' = nextSubmit + 1
    /\ timelineAdmitted' = TRUE
    /\ submittedFullSnapshot' = forceFullReplay
    /\ UNCHANGED <<checkpoint, forceFullReplay, damageCommitted,
                    transportAccepted>>

Accept ==
    /\ phase = Compiled
    /\ phase' = Accepted /\ transportAccepted' = TRUE
    /\ damageCommitted' = TRUE /\ forceFullReplay' = FALSE
    /\ timelineAdmitted' = FALSE
    /\ UNCHANGED <<nextSubmit, checkpoint, submittedFullSnapshot>>

Reject ==
    /\ phase = Compiled
    /\ phase' = Rejected /\ transportAccepted' = FALSE
    /\ nextSubmit' = checkpoint /\ timelineAdmitted' = FALSE
    /\ damageCommitted' = FALSE /\ forceFullReplay' = TRUE
    /\ UNCHANGED <<checkpoint, submittedFullSnapshot>>

Terminal ==
    /\ phase = Accepted
    /\ UNCHANGED vars

Next == TakeCheckpoint \/ Compile \/ Accept \/ Reject \/ Terminal
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {Idle, Checkpointed, Compiled, Accepted, Rejected}
    /\ nextSubmit \in Nat /\ checkpoint \in Nat
    /\ timelineAdmitted \in BOOLEAN /\ forceFullReplay \in BOOLEAN
    /\ submittedFullSnapshot \in BOOLEAN /\ damageCommitted \in BOOLEAN
    /\ transportAccepted \in BOOLEAN

RejectedSubmitRestoresExactCheckpoint ==
    phase = Rejected => nextSubmit = checkpoint /\ ~timelineAdmitted
                        /\ ~damageCommitted /\ forceFullReplay
AcceptedSubmitAloneCommitsDamage ==
    damageCommitted => phase = Accepted /\ transportAccepted
RetryAfterRejectIsFull ==
    phase = Compiled /\ checkpoint > 1 => submittedFullSnapshot

=============================================================================
