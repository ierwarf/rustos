-------------------- MODULE DvmTransportLifecycle --------------------
EXTENDS Naturals, TLC

CONSTANTS MaxClaims, MaxRejected

VARIABLES state, epoch, claims, resetCount, resetWithClaim,
          admittedAfterDrain, rejectedAfterDrain

vars == <<state, epoch, claims, resetCount, resetWithClaim,
          admittedAfterDrain, rejectedAfterDrain>>

Init ==
    /\ state = "Detached"
    /\ epoch = 0
    /\ claims = 0
    /\ resetCount = 0
    /\ resetWithClaim = FALSE
    /\ admittedAfterDrain = FALSE
    /\ rejectedAfterDrain = 0

Activate ==
    /\ state = "Detached"
    /\ state' = "Active"
    /\ epoch' = epoch + 1
    /\ UNCHANGED <<claims, resetCount, resetWithClaim,
                    admittedAfterDrain, rejectedAfterDrain>>

Claim ==
    /\ state = "Active"
    /\ claims < MaxClaims
    /\ claims' = claims + 1
    /\ UNCHANGED <<state, epoch, resetCount, resetWithClaim,
                    admittedAfterDrain, rejectedAfterDrain>>

FinishClaim ==
    /\ claims > 0
    /\ claims' = claims - 1
    /\ UNCHANGED <<state, epoch, resetCount, resetWithClaim,
                    admittedAfterDrain, rejectedAfterDrain>>

BeginDrain ==
    /\ state = "Active"
    /\ state' = "Draining"
    /\ UNCHANGED <<epoch, claims, resetCount, resetWithClaim,
                    admittedAfterDrain, rejectedAfterDrain>>

RejectLateClaim ==
    /\ state = "Draining"
    /\ rejectedAfterDrain < MaxRejected
    /\ rejectedAfterDrain' = rejectedAfterDrain + 1
    /\ UNCHANGED <<state, epoch, claims, resetCount, resetWithClaim,
                    admittedAfterDrain>>

CompleteDrain ==
    /\ state = "Draining"
    /\ claims = 0
    /\ state' = "Revoked"
    /\ resetCount' = resetCount + 1
    /\ resetWithClaim' = (claims # 0)
    /\ UNCHANGED <<epoch, claims, admittedAfterDrain, rejectedAfterDrain>>

Terminal ==
    /\ state = "Revoked"
    /\ UNCHANGED vars

Next == Activate \/ Claim \/ FinishClaim \/ BeginDrain \/ RejectLateClaim
        \/ CompleteDrain \/ Terminal

Spec == Init /\ [][Next]_vars /\ WF_vars(FinishClaim) /\ WF_vars(CompleteDrain)

TypeInvariant ==
    /\ state \in {"Detached", "Active", "Draining", "Revoked"}
    /\ epoch \in Nat
    /\ claims \in 0..MaxClaims
    /\ resetCount \in Nat
    /\ resetWithClaim \in BOOLEAN
    /\ admittedAfterDrain \in BOOLEAN
    /\ rejectedAfterDrain \in 0..MaxRejected

ResetOnlyAfterQuiescence == ~resetWithClaim
DrainClosesAdmission == ~admittedAfterDrain
RevocationIsQuiescent == (state = "Revoked") => (claims = 0)
DrainEventuallyRevokes == (state = "Draining") ~> (state = "Revoked")

=============================================================================
