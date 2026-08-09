-------------------------- MODULE DvmBlockStartup ---------------------------
EXTENDS Naturals

(*******************************************************************************
Composes the asynchronous storage-DVM startup boundary with storaged's first
volume query and the kernel broker's check-arm-recheck sleep.

RustOS admits the fixed BAR2 aperture only as prefetchable shared RAM and maps
the whole atomic header/cursor/payload aperture WB. Linux's prefetchable-BAR
and WB-mmap rules are a static driver contract, but its userspace mapping may
become active after RustOS installation. Revoke clears only RUSTOS_READY. A
later try_install may recover through one strictly newer, zero-cursor, signed,
ready-clear successor header and republishes RUSTOS_READY itself.
*******************************************************************************)

CONSTANTS MaxTime, WaitBound, MaxGeneration,
          LinuxDriverPrefetchable, LinuxDriverCacheMode

Phases == {"uninstalled", "verified", "armed", "ready", "proven", "using",
           "timed-out", "revoked"}
WaitStates == {"idle", "registering", "sleeping", "woken"}
ActivePhases == {"armed", "ready", "proven", "using"}
InstalledPhases == {"verified", "armed", "ready", "proven", "using"}
NoWaiterPhases == {"using", "timed-out", "revoked"}
CacheModes == {"unmapped", "wb", "wc"}

LinuxDriverContractHolds ==
    LinuxDriverPrefetchable /\ LinuxDriverCacheMode = "wb"

VARIABLES phase, rustosReady, peerReady, publicationFailed, waitState,
          observedReady, now, deadline, dataPlaneProven, volumeUsed,
          barPrefetchable, rustosApertureCache, linuxApertureCache,
          generation, successorOffered, candidateGeneration,
          candidateSignatureValid, candidateCursorsZero, candidateReadyClear,
          revokedPeerReady, previousGeneration, rebound, lastRebindWasExact

vars == <<phase, rustosReady, peerReady, publicationFailed, waitState,
          observedReady, now, deadline, dataPlaneProven, volumeUsed,
          barPrefetchable, rustosApertureCache, linuxApertureCache,
          generation, successorOffered, candidateGeneration,
          candidateSignatureValid, candidateCursorsZero, candidateReadyClear,
          revokedPeerReady, previousGeneration, rebound, lastRebindWasExact>>

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
    /\ barPrefetchable = FALSE
    /\ rustosApertureCache = "unmapped"
    /\ linuxApertureCache = "unmapped"
    /\ generation = 1
    /\ successorOffered = FALSE
    /\ candidateGeneration = 0
    /\ candidateSignatureValid = FALSE
    /\ candidateCursorsZero = FALSE
    /\ candidateReadyClear = FALSE
    /\ revokedPeerReady = FALSE
    /\ previousGeneration = 0
    /\ rebound = FALSE
    /\ lastRebindWasExact = FALSE

AdmitPrefetchableRamBar ==
    /\ phase = "uninstalled"
    /\ ~barPrefetchable
    /\ barPrefetchable' = TRUE
    /\ UNCHANGED <<phase, rustosReady, peerReady, publicationFailed, waitState,
                   observedReady, now, deadline, dataPlaneProven, volumeUsed,
                   rustosApertureCache, linuxApertureCache, generation,
                   successorOffered, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, revokedPeerReady, previousGeneration,
                   rebound, lastRebindWasExact>>

MapRustosAtomicApertureWb ==
    /\ phase = "uninstalled"
    /\ rustosApertureCache # "wb"
    /\ rustosApertureCache' = "wb"
    /\ UNCHANGED <<phase, rustosReady, peerReady, publicationFailed, waitState,
                   observedReady, now, deadline, dataPlaneProven, volumeUsed,
                   barPrefetchable, linuxApertureCache, generation,
                   successorOffered, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, revokedPeerReady, previousGeneration,
                   rebound, lastRebindWasExact>>

MapLinuxAtomicApertureWb ==
    /\ phase \notin {"using", "timed-out"}
    /\ LinuxDriverContractHolds
    /\ barPrefetchable
    /\ linuxApertureCache # "wb"
    /\ linuxApertureCache' = "wb"
    /\ UNCHANGED <<phase, rustosReady, peerReady, publicationFailed, waitState,
                   observedReady, now, deadline, dataPlaneProven, volumeUsed,
                   barPrefetchable, rustosApertureCache, generation,
                   successorOffered, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, revokedPeerReady, previousGeneration,
                   rebound, lastRebindWasExact>>

InstallTransport ==
    /\ phase = "uninstalled"
    /\ barPrefetchable
    /\ rustosApertureCache = "wb"
    /\ phase' = "verified"
    /\ UNCHANGED <<rustosReady, peerReady, publicationFailed, waitState,
                   observedReady, now, deadline, dataPlaneProven, volumeUsed,
                   barPrefetchable, rustosApertureCache, linuxApertureCache,
                   generation, successorOffered, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, revokedPeerReady, previousGeneration,
                   rebound, lastRebindWasExact>>

PublishRustosReady ==
    /\ phase = "verified"
    /\ ~peerReady
    /\ phase' = "armed"
    /\ rustosReady' = TRUE
    /\ publicationFailed' = FALSE
    /\ UNCHANGED <<peerReady, waitState, observedReady, now, deadline,
                   dataPlaneProven, volumeUsed, barPrefetchable,
                   rustosApertureCache, linuxApertureCache, generation,
                   successorOffered, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, revokedPeerReady, previousGeneration,
                   rebound, lastRebindWasExact>>

PeerStateRace ==
    /\ phase = "verified"
    /\ LinuxDriverContractHolds
    /\ linuxApertureCache = "wb"
    /\ ~peerReady
    /\ peerReady' = TRUE
    /\ UNCHANGED <<phase, rustosReady, publicationFailed, waitState,
                   observedReady, now, deadline, dataPlaneProven, volumeUsed,
                   barPrefetchable, rustosApertureCache, linuxApertureCache,
                   generation, successorOffered, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, revokedPeerReady, previousGeneration,
                   rebound, lastRebindWasExact>>

RejectRacedPublication ==
    /\ phase = "verified"
    /\ peerReady
    /\ phase' = "revoked"
    /\ publicationFailed' = TRUE
    /\ revokedPeerReady' = peerReady
    /\ UNCHANGED <<rustosReady, peerReady, waitState, observedReady, now,
                   deadline, dataPlaneProven, volumeUsed, barPrefetchable,
                   rustosApertureCache, linuxApertureCache, generation,
                   successorOffered, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, previousGeneration, rebound,
                   lastRebindWasExact>>

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
                   dataPlaneProven, volumeUsed, barPrefetchable,
                   rustosApertureCache, linuxApertureCache, generation,
                   successorOffered, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, revokedPeerReady, previousGeneration,
                   rebound, lastRebindWasExact>>

PublishPeerReady ==
    /\ phase = "armed"
    /\ rustosReady
    /\ LinuxDriverContractHolds
    /\ linuxApertureCache = "wb"
    /\ ~peerReady
    /\ peerReady' = TRUE
    /\ waitState' =
          IF waitState \in {"registering", "sleeping"}
          THEN "woken"
          ELSE waitState
    /\ UNCHANGED <<phase, rustosReady, publicationFailed, observedReady, now,
                   deadline, dataPlaneProven, volumeUsed, barPrefetchable,
                   rustosApertureCache, linuxApertureCache, generation,
                   successorOffered, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, revokedPeerReady, previousGeneration,
                   rebound, lastRebindWasExact>>

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
                   dataPlaneProven, volumeUsed, barPrefetchable,
                   rustosApertureCache, linuxApertureCache, generation,
                   successorOffered, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, revokedPeerReady, previousGeneration,
                   rebound, lastRebindWasExact>>

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
                   dataPlaneProven, volumeUsed, barPrefetchable,
                   rustosApertureCache, linuxApertureCache, generation,
                   successorOffered, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, revokedPeerReady, previousGeneration,
                   rebound, lastRebindWasExact>>

Tick ==
    /\ phase \notin {"using", "timed-out"}
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<phase, rustosReady, peerReady, publicationFailed, waitState,
                   observedReady, deadline, dataPlaneProven, volumeUsed,
                   barPrefetchable, rustosApertureCache, linuxApertureCache,
                   generation, successorOffered, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, revokedPeerReady, previousGeneration,
                   rebound, lastRebindWasExact>>

Timeout ==
    /\ phase = "armed"
    /\ waitState \in {"registering", "sleeping", "woken"}
    /\ now >= deadline
    /\ ~peerReady
    /\ phase' = "timed-out"
    /\ waitState' = "idle"
    /\ UNCHANGED <<rustosReady, peerReady, publicationFailed, observedReady,
                   now, deadline, dataPlaneProven, volumeUsed, barPrefetchable,
                   rustosApertureCache, linuxApertureCache, generation,
                   successorOffered, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, revokedPeerReady, previousGeneration,
                   rebound, lastRebindWasExact>>

ProveDataPlane ==
    /\ phase = "ready"
    /\ peerReady
    /\ observedReady
    /\ LinuxDriverContractHolds
    /\ linuxApertureCache = "wb"
    /\ phase' = "proven"
    /\ dataPlaneProven' = TRUE
    /\ UNCHANGED <<rustosReady, peerReady, publicationFailed, waitState,
                   observedReady, now, deadline, volumeUsed, barPrefetchable,
                   rustosApertureCache, linuxApertureCache, generation,
                   successorOffered, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, revokedPeerReady, previousGeneration,
                   rebound, lastRebindWasExact>>

UseVolume ==
    /\ phase = "proven"
    /\ peerReady
    /\ observedReady
    /\ dataPlaneProven
    /\ phase' = "using"
    /\ volumeUsed' = TRUE
    /\ UNCHANGED <<rustosReady, peerReady, publicationFailed, waitState,
                   observedReady, now, deadline, dataPlaneProven,
                   barPrefetchable, rustosApertureCache, linuxApertureCache,
                   generation, successorOffered, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, revokedPeerReady, previousGeneration,
                   rebound, lastRebindWasExact>>

Revoke ==
    /\ phase \in {"armed", "ready", "proven", "using"}
    /\ phase' = "revoked"
    /\ rustosReady' = FALSE
    /\ waitState' = "idle"
    /\ dataPlaneProven' = FALSE
    /\ volumeUsed' = FALSE
    /\ successorOffered' = FALSE
    /\ revokedPeerReady' = peerReady
    /\ UNCHANGED <<peerReady, publicationFailed, observedReady, now, deadline,
                   barPrefetchable, rustosApertureCache,
                   linuxApertureCache, generation, candidateGeneration,
                   candidateSignatureValid, candidateCursorsZero,
                   candidateReadyClear, previousGeneration, rebound,
                   lastRebindWasExact>>

OfferSuccessor(candidate, signatureOk, zeroCursors, readyClear) ==
    /\ phase = "revoked"
    /\ successorOffered' = TRUE
    /\ candidateGeneration' = candidate
    /\ candidateSignatureValid' = signatureOk
    /\ candidateCursorsZero' = zeroCursors
    /\ candidateReadyClear' = readyClear
    /\ rustosReady' = IF readyClear THEN FALSE ELSE rustosReady
    /\ peerReady' = IF readyClear THEN FALSE ELSE peerReady
    /\ observedReady' = IF readyClear THEN FALSE ELSE observedReady
    /\ linuxApertureCache' =
          IF readyClear THEN "unmapped" ELSE linuxApertureCache
    /\ UNCHANGED <<phase, publicationFailed, waitState, now, deadline,
                   dataPlaneProven, volumeUsed, barPrefetchable,
                   rustosApertureCache, generation, revokedPeerReady,
                   previousGeneration, rebound, lastRebindWasExact>>

RebindSignedSuccessor ==
    /\ phase = "revoked"
    /\ successorOffered
    /\ candidateGeneration > generation
    /\ candidateCursorsZero
    /\ candidateSignatureValid
    /\ candidateReadyClear
    /\ ~rustosReady
    /\ ~peerReady
    /\ phase' = "armed"
    /\ rustosReady' = TRUE
    /\ publicationFailed' = FALSE
    /\ waitState' = "idle"
    /\ observedReady' = FALSE
    /\ dataPlaneProven' = FALSE
    /\ volumeUsed' = FALSE
    /\ generation' = candidateGeneration
    /\ successorOffered' = FALSE
    /\ previousGeneration' = generation
    /\ rebound' = TRUE
    /\ lastRebindWasExact' =
          (candidateGeneration > generation /\ candidateCursorsZero
           /\ candidateSignatureValid /\ candidateReadyClear)
    /\ UNCHANGED <<peerReady, now, deadline, barPrefetchable,
                   rustosApertureCache, linuxApertureCache,
                   candidateGeneration, candidateSignatureValid,
                   candidateCursorsZero, candidateReadyClear,
                   revokedPeerReady>>

Next ==
    AdmitPrefetchableRamBar
    \/ MapRustosAtomicApertureWb
    \/ MapLinuxAtomicApertureWb
    \/ InstallTransport
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
    \/ \E candidate \in 1..MaxGeneration,
          signatureOk \in BOOLEAN,
          zeroCursors \in BOOLEAN,
          readyClear \in BOOLEAN:
            OfferSuccessor(candidate, signatureOk, zeroCursors, readyClear)
    \/ RebindSignedSuccessor

TypeOK ==
    /\ MaxGeneration \in (Nat \ {0})
    /\ LinuxDriverPrefetchable \in BOOLEAN
    /\ LinuxDriverCacheMode \in CacheModes
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
    /\ barPrefetchable \in BOOLEAN
    /\ rustosApertureCache \in CacheModes
    /\ linuxApertureCache \in CacheModes
    /\ generation \in 1..MaxGeneration
    /\ successorOffered \in BOOLEAN
    /\ candidateGeneration \in 0..MaxGeneration
    /\ candidateSignatureValid \in BOOLEAN
    /\ candidateCursorsZero \in BOOLEAN
    /\ candidateReadyClear \in BOOLEAN
    /\ revokedPeerReady \in BOOLEAN
    /\ previousGeneration \in 0..MaxGeneration
    /\ rebound \in BOOLEAN
    /\ lastRebindWasExact \in BOOLEAN

LinuxDriverContractIsPrefetchableWb == LinuxDriverContractHolds

InstalledTransportHasPrefetchableRustosWbAperture ==
    phase \in InstalledPhases =>
        barPrefetchable /\ rustosApertureCache = "wb"

PeerReadyRequiresActiveLinuxWb ==
    peerReady => LinuxDriverContractHolds /\ linuxApertureCache = "wb"

RustosAtomicApertureNeverUsesWc == rustosApertureCache # "wc"

WcRustosAliasCannotInstallOrBecomeReady ==
    rustosApertureCache = "wc" =>
        /\ phase = "uninstalled"
        /\ ~rustosReady
        /\ ~peerReady
        /\ ~volumeUsed

RevokedStateClearsRustosReady == phase = "revoked" => ~rustosReady

RevokePreservesPeerReadyUntilSuccessorHeader ==
    phase = "revoked" /\ ~successorOffered => peerReady = revokedPeerReady

ReboundEpochIsStrictlyNewerSignedZeroCursor ==
    rebound => generation > previousGeneration /\ lastRebindWasExact

RecoveredActiveEpochHasRustosReady ==
    rebound /\ phase \in ActivePhases => rustosReady

NoUseBeforeReadiness ==
    volumeUsed =>
        /\ rustosReady
        /\ peerReady
        /\ observedReady
        /\ dataPlaneProven
        /\ linuxApertureCache = "wb"
        /\ phase = "using"

FailedPublicationLeavesNoReadyBit == publicationFailed => ~rustosReady

AdmittedPeerFollowsRustosReadiness ==
    phase \in ActivePhases /\ peerReady => rustosReady

InitialNotReadyIsNonterminal ==
    phase = "armed" /\ ~peerReady => ~volumeUsed

ReadyRequiresExactObservation ==
    phase \in {"ready", "proven", "using"} => peerReady /\ observedReady

ProofRequiresExactReadiness ==
    dataPlaneProven =>
        /\ peerReady
        /\ observedReady
        /\ linuxApertureCache = "wb"
        /\ phase \in {"proven", "using"}

SleepingRequiresALiveDeadline ==
    waitState = "sleeping" => phase = "armed" /\ deadline > 0

InactivePhaseHasNoWaiter == phase \in NoWaiterPhases => waitState = "idle"

Spec == Init /\ [][Next]_vars
=============================================================================
