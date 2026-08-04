-------------------- MODULE DvmTransportLifecycle --------------------
EXTENDS Naturals, TLC

CONSTANTS MaxClaims, MaxRejected, MaxEpoch

VARIABLES state, epoch, claims, mappingPublished, activationCancelled,
          resetCount, resetWithClaim, admittedAfterDrain, rejectedAfterDrain

vars == <<state, epoch, claims, mappingPublished, activationCancelled,
          resetCount, resetWithClaim, admittedAfterDrain, rejectedAfterDrain>>

Init ==
    /\ state = "Detached"
    /\ epoch = 0
    /\ claims = 0
    /\ mappingPublished = FALSE
    /\ activationCancelled = FALSE
    /\ resetCount = 0
    /\ resetWithClaim = FALSE
    /\ admittedAfterDrain = FALSE
    /\ rejectedAfterDrain = 0

BeginActivation ==
    /\ state \in {"Detached", "Revoked"}
    /\ claims = 0
    /\ epoch < MaxEpoch
    /\ state' = "Activating"
    /\ epoch' = epoch + 1
    /\ mappingPublished' = FALSE
    /\ activationCancelled' = FALSE
    /\ UNCHANGED <<claims, resetCount, resetWithClaim,
                    admittedAfterDrain, rejectedAfterDrain>>

PublishMapping ==
    /\ state = "Activating"
    /\ ~mappingPublished
    /\ mappingPublished' = TRUE
    /\ UNCHANGED <<state, epoch, claims, activationCancelled, resetCount,
                    resetWithClaim, admittedAfterDrain, rejectedAfterDrain>>

CommitActivation ==
    /\ state = "Activating"
    /\ mappingPublished
    /\ state' = "Active"
    /\ UNCHANGED <<epoch, claims, mappingPublished, activationCancelled,
                    resetCount, resetWithClaim, admittedAfterDrain,
                    rejectedAfterDrain>>

CancelActivation ==
    /\ state = "Activating"
    /\ state' = "Revoked"
    /\ mappingPublished' = FALSE
    /\ activationCancelled' = TRUE
    /\ UNCHANGED <<epoch, claims, resetCount, resetWithClaim,
                    admittedAfterDrain, rejectedAfterDrain>>

Claim ==
    /\ state = "Active"
    /\ mappingPublished
    /\ claims < MaxClaims
    /\ claims' = claims + 1
    /\ UNCHANGED <<state, epoch, mappingPublished, activationCancelled,
                    resetCount, resetWithClaim, admittedAfterDrain,
                    rejectedAfterDrain>>

FinishClaim ==
    /\ claims > 0
    /\ claims' = claims - 1
    /\ UNCHANGED <<state, epoch, mappingPublished, activationCancelled,
                    resetCount, resetWithClaim, admittedAfterDrain,
                    rejectedAfterDrain>>

BeginDrain ==
    /\ state \in {"Active", "Activating"}
    /\ state' = "Draining"
    /\ activationCancelled' = (state = "Activating")
    /\ UNCHANGED <<epoch, claims, mappingPublished, resetCount,
                    resetWithClaim, admittedAfterDrain, rejectedAfterDrain>>

RejectLateClaim ==
    /\ state = "Draining"
    /\ rejectedAfterDrain < MaxRejected
    /\ rejectedAfterDrain' = rejectedAfterDrain + 1
    /\ UNCHANGED <<state, epoch, claims, mappingPublished,
                    activationCancelled, resetCount, resetWithClaim,
                    admittedAfterDrain>>

CompleteDrain ==
    /\ state = "Draining"
    /\ claims = 0
    /\ state' = "Revoked"
    /\ mappingPublished' = FALSE
    /\ resetCount' = resetCount + 1
    /\ resetWithClaim' = (claims # 0)
    /\ UNCHANGED <<epoch, claims, activationCancelled,
                    admittedAfterDrain, rejectedAfterDrain>>

Terminal ==
    /\ state = "Revoked"
    /\ UNCHANGED vars

Next == BeginActivation \/ PublishMapping \/ CommitActivation \/ CancelActivation
        \/ Claim \/ FinishClaim \/ BeginDrain \/ RejectLateClaim
        \/ CompleteDrain \/ Terminal

Spec == Init /\ [][Next]_vars /\ WF_vars(FinishClaim) /\ WF_vars(CompleteDrain)

TypeInvariant ==
    /\ state \in {"Detached", "Activating", "Active", "Draining", "Revoked"}
    /\ epoch \in 0..MaxEpoch
    /\ claims \in 0..MaxClaims
    /\ mappingPublished \in BOOLEAN
    /\ activationCancelled \in BOOLEAN
    /\ resetCount \in Nat
    /\ resetWithClaim \in BOOLEAN
    /\ admittedAfterDrain \in BOOLEAN
    /\ rejectedAfterDrain \in 0..MaxRejected

ActiveRequiresPublishedMapping == (state = "Active") => mappingPublished
RevokedOwnsNoMapping == (state = "Revoked") => ~mappingPublished
ResetOnlyAfterQuiescence == ~resetWithClaim
DrainClosesAdmission == ~admittedAfterDrain
RevocationIsQuiescent == (state = "Revoked") => (claims = 0)
DrainCancelsActivation == activationCancelled => state # "Active"
DrainEventuallyRevokes == (state = "Draining") ~> (state = "Revoked")

=============================================================================
