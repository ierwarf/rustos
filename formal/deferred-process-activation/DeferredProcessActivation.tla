---------------------- MODULE DeferredProcessActivation ----------------------
EXTENDS Naturals

(*******************************************************************************
Models the one-shot authority created by a deferred loader spawn.

Loaderd stamps the kernel-observed requester's PID, but does not retain the
authority: ring0 binds the suspended target to that requester. Foreign use is
rejected, loaderd restart cannot erase or transfer the binding, requester exit
revokes an unconsumed binding and target. Successful use moves the original
non-Copy authority out of the registry only after scheduler preflight. The
registry leaf ends before runnable publication; the original grant remains held
through that last effect. Requester cleanup cannot steal an already moved grant.
*******************************************************************************)

CONSTANTS Requester, Foreign, MaxLoaderEpoch, MaxDenied

ASSUME /\ Requester # 0
       /\ Foreign # 0
       /\ Requester # Foreign
       /\ MaxLoaderEpoch > 0
       /\ MaxDenied > 0

Phases == {"absent", "suspended", "accepted", "publishing", "active", "revoked", "exited"}

VARIABLES phase, requesterLive, activationOwner, running, consumed,
          loaderEpoch, denied, grantHeld, registryHeld

vars == <<phase, requesterLive, activationOwner, running, consumed,
          loaderEpoch, denied, grantHeld, registryHeld>>

Init ==
    /\ phase = "absent"
    /\ requesterLive = TRUE
    /\ activationOwner = 0
    /\ running = FALSE
    /\ consumed = FALSE
    /\ loaderEpoch = 0
    /\ denied = 0
    /\ grantHeld = FALSE
    /\ registryHeld = FALSE

DeferredSpawn ==
    /\ phase = "absent"
    /\ requesterLive
    /\ phase' = "suspended"
    /\ activationOwner' = Requester
    /\ UNCHANGED <<requesterLive, running, consumed, loaderEpoch, denied,
                   grantHeld, registryHeld>>

ForeignActivate ==
    /\ phase = "suspended"
    /\ activationOwner = Requester
    /\ denied < MaxDenied
    /\ denied' = denied + 1
    /\ UNCHANGED <<phase, requesterLive, activationOwner, running, consumed,
                   loaderEpoch, grantHeld, registryHeld>>

RestartLoader ==
    /\ phase \in {"suspended", "accepted", "publishing", "active"}
    /\ loaderEpoch < MaxLoaderEpoch
    /\ loaderEpoch' = loaderEpoch + 1
    /\ UNCHANGED <<phase, requesterLive, activationOwner, running, consumed,
                   denied, grantHeld, registryHeld>>

OwnerActivate ==
    /\ phase = "suspended"
    /\ requesterLive
    /\ activationOwner = Requester
    /\ phase' = "accepted"
    /\ activationOwner' = 0
    /\ grantHeld' = TRUE
    /\ registryHeld' = TRUE
    /\ consumed' = TRUE
    /\ UNCHANGED <<requesterLive, running, loaderEpoch, denied>>

ReleaseRegistry ==
    /\ phase = "accepted"
    /\ phase' = "publishing"
    /\ registryHeld' = FALSE
    /\ UNCHANGED <<requesterLive, activationOwner, running, consumed,
                   loaderEpoch, denied, grantHeld>>

PublishRunnable ==
    /\ phase = "publishing"
    /\ grantHeld
    /\ ~registryHeld
    /\ phase' = "active"
    /\ running' = TRUE
    /\ UNCHANGED <<requesterLive, activationOwner, consumed, loaderEpoch,
                   denied, grantHeld, registryHeld>>

ReleaseOriginalGrant ==
    /\ phase \in {"active", "exited"}
    /\ grantHeld
    /\ grantHeld' = FALSE
    /\ UNCHANGED <<phase, requesterLive, activationOwner, running, consumed,
                   loaderEpoch, denied, registryHeld>>

RequesterExitBeforeUse ==
    /\ phase = "suspended"
    /\ requesterLive
    /\ phase' = "revoked"
    /\ requesterLive' = FALSE
    /\ activationOwner' = 0
    /\ UNCHANGED <<running, consumed, loaderEpoch, denied, grantHeld, registryHeld>>

RequesterExitAfterUse ==
    /\ phase \in {"accepted", "publishing", "active"}
    /\ requesterLive
    /\ requesterLive' = FALSE
    /\ UNCHANGED <<phase, activationOwner, running, consumed, loaderEpoch,
                   denied, grantHeld, registryHeld>>

TargetExit ==
    /\ phase = "active"
    /\ phase' = "exited"
    /\ running' = FALSE
    /\ UNCHANGED <<requesterLive, activationOwner, consumed, loaderEpoch,
                   denied, grantHeld, registryHeld>>

Next ==
    DeferredSpawn
    \/ ForeignActivate
    \/ RestartLoader
    \/ OwnerActivate
    \/ ReleaseRegistry
    \/ PublishRunnable
    \/ ReleaseOriginalGrant
    \/ RequesterExitBeforeUse
    \/ RequesterExitAfterUse
    \/ TargetExit

TypeOK ==
    /\ phase \in Phases
    /\ requesterLive \in BOOLEAN
    /\ activationOwner \in {0, Requester}
    /\ running \in BOOLEAN
    /\ consumed \in BOOLEAN
    /\ grantHeld \in BOOLEAN
    /\ registryHeld \in BOOLEAN
    /\ loaderEpoch \in 0..MaxLoaderEpoch
    /\ denied \in 0..MaxDenied

SuspendedHasExactOwner ==
    phase = "suspended" => requesterLive /\ activationOwner = Requester

RunningRequiresConsumedAuthority ==
    running => phase = "active" /\ consumed /\ activationOwner = 0

ConsumedAuthorityIsOneShot ==
    consumed => activationOwner = 0 /\ phase \in {"accepted", "publishing", "active", "exited"}

RevokedTargetIsInert ==
    phase = "revoked" => ~running /\ activationOwner = 0

OriginalGrantThroughLastEffect ==
    phase \in {"accepted", "publishing"} => grantHeld /\ consumed

RegistryLeafEndsBeforePublication ==
    registryHeld <=> phase = "accepted"

Spec == Init /\ [][Next]_vars
=============================================================================
