-------------------------- MODULE DvmBlockStartup ---------------------------
EXTENDS Naturals

(*******************************************************************************
Composes the asynchronous storage-DVM startup boundary with storaged's first
volume query and the kernel broker's check-arm-recheck sleep.

The RustOS ivshmem transport is installed before Linux has necessarily bound
its controller.  DVM_READY=0 is therefore a sleepable startup state, not a
fault and not permission to fall back to bootstrap storage.  Readiness may be
published before the first check, between check and waiter registration, or
after the scheduler block; every ordering is resolved by a final recheck.
Timeout and revoke are explicit terminal outcomes.
*******************************************************************************)

CONSTANTS MaxTime, WaitBound

Phases == {"uninstalled", "verified", "armed", "ready", "proven", "using",
           "timed-out", "revoked"}
WaitStates == {"idle", "registering", "sleeping", "woken"}
TerminalPhases == {"using", "timed-out", "revoked"}

VARIABLES phase, rustosReady, peerReady, publicationFailed, waitState,
          observedReady, now, deadline, dataPlaneProven, volumeUsed

vars == <<phase, rustosReady, peerReady, publicationFailed, waitState,
          observedReady, now, deadline, dataPlaneProven, volumeUsed>>

Init ==
    /\ phase = "uninstalled"
    /\ rustosReady = FALSE
    /\ peerReady = FALSE
    /\ publicationFailed = FALSE
    /\ waitState = "idle"
    /\ observedReady = FALSE
    /\ now = 0
    /\ deadline = 0
    /\ dataPlaneProven = FALSE
    /\ volumeUsed = FALSE

InstallTransport ==
    /\ phase = "uninstalled"
    /\ phase' = "verified"
    /\ UNCHANGED <<rustosReady, peerReady, publicationFailed, waitState,
                   observedReady, now, deadline, dataPlaneProven, volumeUsed>>

PublishRustosReady ==
    /\ phase = "verified"
    /\ ~peerReady
    /\ phase' = "armed"
    /\ rustosReady' = TRUE
    /\ publicationFailed' = FALSE
    /\ UNCHANGED <<peerReady, waitState, observedReady, now, deadline,
                   dataPlaneProven, volumeUsed>>

PeerStateRace ==
    /\ phase = "verified"
    /\ ~peerReady
    /\ peerReady' = TRUE
    /\ UNCHANGED <<phase, rustosReady, publicationFailed, waitState,
                   observedReady, now, deadline, dataPlaneProven, volumeUsed>>

RejectRacedPublication ==
    /\ phase = "verified"
    /\ peerReady
    /\ phase' = "revoked"
    /\ publicationFailed' = TRUE
    /\ UNCHANGED <<rustosReady, peerReady, waitState, observedReady, now,
                   deadline, dataPlaneProven, volumeUsed>>

BeginInfo ==
    /\ phase = "armed"
    /\ waitState = "idle"
    /\ now + WaitBound <= MaxTime
    /\ deadline' = now + WaitBound
    /\ IF peerReady
          THEN /\ phase' = "ready"
               /\ observedReady' = TRUE
               /\ waitState' = "idle"
          ELSE /\ phase' = phase
               /\ observedReady' = FALSE
               /\ waitState' = "registering"
    /\ UNCHANGED <<rustosReady, peerReady, publicationFailed, now,
                   dataPlaneProven, volumeUsed>>

PublishPeerReady ==
    /\ phase = "armed"
    /\ rustosReady
    /\ ~peerReady
    /\ peerReady' = TRUE
    /\ waitState' =
          IF waitState \in {"registering", "sleeping"}
          THEN "woken"
          ELSE waitState
    /\ UNCHANGED <<phase, rustosReady, publicationFailed, observedReady, now,
                   deadline, dataPlaneProven, volumeUsed>>

ArmRecheck ==
    /\ phase = "armed"
    /\ waitState = "registering"
    /\ IF peerReady
          THEN /\ phase' = "ready"
               /\ observedReady' = TRUE
               /\ waitState' = "idle"
          ELSE /\ phase' = phase
               /\ observedReady' = observedReady
               /\ waitState' = "sleeping"
    /\ UNCHANGED <<rustosReady, peerReady, publicationFailed, now, deadline,
                   dataPlaneProven, volumeUsed>>

ResolveWake ==
    /\ phase = "armed"
    /\ waitState = "woken"
    /\ IF peerReady
          THEN /\ phase' = "ready"
               /\ observedReady' = TRUE
               /\ waitState' = "idle"
          ELSE /\ phase' = phase
               /\ observedReady' = observedReady
               /\ waitState' = "registering"
    /\ UNCHANGED <<rustosReady, peerReady, publicationFailed, now, deadline,
                   dataPlaneProven, volumeUsed>>

Tick ==
    /\ phase \notin TerminalPhases
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<phase, rustosReady, peerReady, publicationFailed, waitState,
                   observedReady, deadline, dataPlaneProven, volumeUsed>>

Timeout ==
    /\ phase = "armed"
    /\ waitState \in {"registering", "sleeping", "woken"}
    /\ now >= deadline
    /\ ~peerReady
    /\ phase' = "timed-out"
    /\ waitState' = "idle"
    /\ UNCHANGED <<rustosReady, peerReady, publicationFailed, observedReady,
                   now, deadline, dataPlaneProven, volumeUsed>>

ProveDataPlane ==
    /\ phase = "ready"
    /\ peerReady
    /\ observedReady
    /\ phase' = "proven"
    /\ dataPlaneProven' = TRUE
    /\ UNCHANGED <<rustosReady, peerReady, publicationFailed, waitState,
                   observedReady, now, deadline, volumeUsed>>

UseVolume ==
    /\ phase = "proven"
    /\ peerReady
    /\ observedReady
    /\ dataPlaneProven
    /\ phase' = "using"
    /\ volumeUsed' = TRUE
    /\ UNCHANGED <<rustosReady, peerReady, publicationFailed, waitState,
                   observedReady, now, deadline, dataPlaneProven>>

Revoke ==
    /\ phase \in {"armed", "ready", "proven"}
    /\ phase' = "revoked"
    /\ rustosReady' = FALSE
    /\ peerReady' = FALSE
    /\ waitState' = "idle"
    /\ dataPlaneProven' = FALSE
    /\ UNCHANGED <<publicationFailed, observedReady, now, deadline, volumeUsed>>

Next ==
    InstallTransport
    \/ PublishRustosReady
    \/ PeerStateRace
    \/ RejectRacedPublication
    \/ BeginInfo
    \/ PublishPeerReady
    \/ ArmRecheck
    \/ ResolveWake
    \/ Tick
    \/ Timeout
    \/ ProveDataPlane
    \/ UseVolume
    \/ Revoke

TypeOK ==
    /\ phase \in Phases
    /\ rustosReady \in BOOLEAN
    /\ peerReady \in BOOLEAN
    /\ publicationFailed \in BOOLEAN
    /\ waitState \in WaitStates
    /\ observedReady \in BOOLEAN
    /\ now \in 0..MaxTime
    /\ deadline \in 0..MaxTime
    /\ dataPlaneProven \in BOOLEAN
    /\ volumeUsed \in BOOLEAN

NoUseBeforeReadiness ==
    volumeUsed =>
        rustosReady /\ peerReady /\ observedReady /\ dataPlaneProven /\ phase = "using"

FailedPublicationLeavesNoReadyBit ==
    publicationFailed => ~rustosReady

AdmittedPeerFollowsRustosReadiness ==
    phase \in {"armed", "ready", "proven", "using"} /\ peerReady => rustosReady

InitialNotReadyIsNonterminal ==
    phase = "armed" /\ ~peerReady => ~volumeUsed

ReadyRequiresExactObservation ==
    phase \in {"ready", "proven", "using"} => peerReady /\ observedReady

ProofRequiresExactReadiness ==
    dataPlaneProven => peerReady /\ observedReady /\ phase \in {"proven", "using"}

SleepingRequiresALiveDeadline ==
    waitState = "sleeping" => phase = "armed" /\ deadline > 0

TerminalHasNoWaiter ==
    phase \in TerminalPhases => waitState = "idle"

Spec == Init /\ [][Next]_vars
=============================================================================
