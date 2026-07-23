---------------------- MODULE DeferredProcessActivation ----------------------
EXTENDS Naturals

(*******************************************************************************
Models the one-shot authority created by a deferred loader spawn.

Loaderd stamps the kernel-observed requester's PID, but does not retain the
authority: ring0 binds the suspended target to that requester. Foreign use is
rejected, loaderd restart cannot erase or transfer the binding, requester exit
revokes the binding and target, and successful use consumes the authority
before the target becomes runnable.
*******************************************************************************)

CONSTANTS Requester, Foreign, MaxLoaderEpoch, MaxDenied

ASSUME /\ Requester # 0
       /\ Foreign # 0
       /\ Requester # Foreign
       /\ MaxLoaderEpoch > 0
       /\ MaxDenied > 0

Phases == {"absent", "suspended", "active", "revoked", "exited"}

VARIABLES phase, requesterLive, activationOwner, running, consumed,
          loaderEpoch, denied

vars == <<phase, requesterLive, activationOwner, running, consumed,
          loaderEpoch, denied>>

Init ==
    /\ phase = "absent"
    /\ requesterLive = TRUE
    /\ activationOwner = 0
    /\ running = FALSE
    /\ consumed = FALSE
    /\ loaderEpoch = 0
    /\ denied = 0

DeferredSpawn ==
    /\ phase = "absent"
    /\ requesterLive
    /\ phase' = "suspended"
    /\ activationOwner' = Requester
    /\ UNCHANGED <<requesterLive, running, consumed, loaderEpoch, denied>>

ForeignActivate ==
    /\ phase = "suspended"
    /\ activationOwner = Requester
    /\ denied < MaxDenied
    /\ denied' = denied + 1
    /\ UNCHANGED <<phase, requesterLive, activationOwner, running, consumed,
                   loaderEpoch>>

RestartLoader ==
    /\ phase \in {"suspended", "active"}
    /\ loaderEpoch < MaxLoaderEpoch
    /\ loaderEpoch' = loaderEpoch + 1
    /\ UNCHANGED <<phase, requesterLive, activationOwner, running, consumed,
                   denied>>

OwnerActivate ==
    /\ phase = "suspended"
    /\ requesterLive
    /\ activationOwner = Requester
    /\ phase' = "active"
    /\ activationOwner' = 0
    /\ running' = TRUE
    /\ consumed' = TRUE
    /\ UNCHANGED <<requesterLive, loaderEpoch, denied>>

RequesterExitBeforeUse ==
    /\ phase = "suspended"
    /\ requesterLive
    /\ phase' = "revoked"
    /\ requesterLive' = FALSE
    /\ activationOwner' = 0
    /\ UNCHANGED <<running, consumed, loaderEpoch, denied>>

RequesterExitAfterUse ==
    /\ phase = "active"
    /\ requesterLive
    /\ requesterLive' = FALSE
    /\ UNCHANGED <<phase, activationOwner, running, consumed, loaderEpoch,
                   denied>>

TargetExit ==
    /\ phase = "active"
    /\ phase' = "exited"
    /\ running' = FALSE
    /\ UNCHANGED <<requesterLive, activationOwner, consumed, loaderEpoch,
                   denied>>

Next ==
    DeferredSpawn
    \/ ForeignActivate
    \/ RestartLoader
    \/ OwnerActivate
    \/ RequesterExitBeforeUse
    \/ RequesterExitAfterUse
    \/ TargetExit

TypeOK ==
    /\ phase \in Phases
    /\ requesterLive \in BOOLEAN
    /\ activationOwner \in {0, Requester}
    /\ running \in BOOLEAN
    /\ consumed \in BOOLEAN
    /\ loaderEpoch \in 0..MaxLoaderEpoch
    /\ denied \in 0..MaxDenied

SuspendedHasExactOwner ==
    phase = "suspended" => requesterLive /\ activationOwner = Requester

RunningRequiresConsumedAuthority ==
    running => phase = "active" /\ consumed /\ activationOwner = 0

ConsumedAuthorityIsOneShot ==
    consumed => activationOwner = 0 /\ phase \in {"active", "exited"}

RevokedTargetIsInert ==
    phase = "revoked" => ~running /\ activationOwner = 0

Spec == Init /\ [][Next]_vars
=============================================================================
