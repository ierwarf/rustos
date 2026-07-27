------------------------------ MODULE ProductBoot ------------------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Composition contract for the product boot topologies exercised by xtask.
Display-provider admission and the storage data-plane proof are independent
branches after input policy is live.  Snapshot activation depends on storage;
the user-visible terminal joins the snapshot and display branches.  This DAG
matches the real concurrent bootstrap instead of imposing a false total order.
***************************************************************************)

CONSTANTS CoreDeadline, InputDeadline, DisplayDeadline, StorageDeadline,
          SnapshotDeadline, FrameDeadline, HardDeadline

Interactive == "interactive"
StorageOnly == "storage-only"
Modes == {Interactive, StorageOnly}

Start == "start"
CoreReady == "core-ready"
InputReady == "input-ready"
SnapshotReady == "snapshot-ready"
ImageActive == "image-active"
WaylandReady == "wayland-ready"
Presented == "presented"
StorageUsable == "storage-usable"
Failed == "failed"
Revoked == "revoked"

Terminal == {Presented, StorageUsable, Failed, Revoked}
InteractivePreterminal ==
    {InputReady, SnapshotReady, ImageActive, WaylandReady}

VARIABLES mode, phase, now, displayProven, snapshotSealed, imageCommitted,
          appActive, waylandConnected, firstFrame, storageProven

vars == <<mode, phase, now, displayProven, snapshotSealed, imageCommitted,
          appActive, waylandConnected, firstFrame, storageProven>>

Init ==
    /\ mode \in Modes
    /\ phase = Start
    /\ now = 0
    /\ displayProven = FALSE
    /\ snapshotSealed = FALSE
    /\ imageCommitted = FALSE
    /\ appActive = FALSE
    /\ waylandConnected = FALSE
    /\ firstFrame = FALSE
    /\ storageProven = FALSE

CoreServicesReady ==
    /\ phase = Start
    /\ now <= CoreDeadline
    /\ phase' = CoreReady
    /\ UNCHANGED <<mode, now, displayProven, snapshotSealed, imageCommitted,
                  appActive, waylandConnected, firstFrame, storageProven>>

InputPolicyReady ==
    /\ mode = Interactive
    /\ phase = CoreReady
    /\ now <= InputDeadline
    /\ phase' = InputReady
    /\ UNCHANGED <<mode, now, displayProven, snapshotSealed, imageCommitted,
                  appActive, waylandConnected, firstFrame, storageProven>>

DisplayProviderReady ==
    /\ mode = Interactive
    /\ phase \in InteractivePreterminal
    /\ ~displayProven
    /\ now <= DisplayDeadline
    /\ displayProven' = TRUE
    /\ UNCHANGED <<mode, phase, now, snapshotSealed, imageCommitted, appActive,
                  waylandConnected, firstFrame, storageProven>>

StorageDataPlaneReady ==
    /\ mode = Interactive
    /\ phase = InputReady
    /\ ~storageProven
    /\ now <= StorageDeadline
    /\ storageProven' = TRUE
    /\ UNCHANGED <<mode, phase, now, displayProven, snapshotSealed,
                  imageCommitted, appActive, waylandConnected, firstFrame>>

StorageOnlyDataPlaneReady ==
    /\ mode = StorageOnly
    /\ phase = CoreReady
    /\ now <= StorageDeadline
    /\ phase' = StorageUsable
    /\ storageProven' = TRUE
    /\ UNCHANGED <<mode, now, displayProven, snapshotSealed, imageCommitted,
                  appActive, waylandConnected, firstFrame>>

SealExecutableSnapshot ==
    /\ mode = Interactive
    /\ phase = InputReady
    /\ storageProven
    /\ now <= SnapshotDeadline
    /\ phase' = SnapshotReady
    /\ snapshotSealed' = TRUE
    /\ UNCHANGED <<mode, now, displayProven, imageCommitted, appActive,
                  waylandConnected, firstFrame, storageProven>>

CommitAndActivateImage ==
    /\ mode = Interactive
    /\ phase = SnapshotReady
    /\ snapshotSealed
    /\ now <= FrameDeadline
    /\ phase' = ImageActive
    /\ imageCommitted' = TRUE
    /\ appActive' = TRUE
    /\ UNCHANGED <<mode, now, displayProven, snapshotSealed, waylandConnected,
                  firstFrame, storageProven>>

ConnectWayland ==
    /\ mode = Interactive
    /\ phase = ImageActive
    /\ imageCommitted
    /\ appActive
    /\ now <= FrameDeadline
    /\ phase' = WaylandReady
    /\ waylandConnected' = TRUE
    /\ UNCHANGED <<mode, now, displayProven, snapshotSealed, imageCommitted,
                  appActive, firstFrame, storageProven>>

PresentFirstFrame ==
    /\ mode = Interactive
    /\ phase = WaylandReady
    /\ displayProven
    /\ storageProven
    /\ snapshotSealed
    /\ imageCommitted
    /\ appActive
    /\ waylandConnected
    /\ now <= FrameDeadline
    /\ phase' = Presented
    /\ firstFrame' = TRUE
    /\ UNCHANGED <<mode, now, displayProven, snapshotSealed, imageCommitted,
                  appActive, waylandConnected, storageProven>>

DeadlineMissed ==
    \/ /\ phase = Start
       /\ now >= CoreDeadline
    \/ /\ phase = CoreReady
       /\ mode = Interactive
       /\ now >= InputDeadline
    \/ /\ mode = StorageOnly
       /\ phase = CoreReady
       /\ now >= StorageDeadline
    \/ /\ mode = Interactive
       /\ phase \in InteractivePreterminal
       /\ ~displayProven
       /\ now >= DisplayDeadline
    \/ /\ mode = Interactive
       /\ phase = InputReady
       /\ ~storageProven
       /\ now >= StorageDeadline
    \/ /\ mode = Interactive
       /\ phase = InputReady
       /\ ~snapshotSealed
       /\ now >= SnapshotDeadline
    \/ /\ mode = Interactive
       /\ phase \in {SnapshotReady, ImageActive, WaylandReady}
       /\ now >= FrameDeadline

Expire ==
    /\ phase \notin Terminal
    /\ DeadlineMissed
    /\ phase' = Failed
    /\ UNCHANGED <<mode, now, displayProven, snapshotSealed, imageCommitted,
                  appActive, waylandConnected, firstFrame, storageProven>>

Revoke ==
    /\ phase \notin Terminal
    /\ phase' = Revoked
    /\ UNCHANGED <<mode, now, displayProven, snapshotSealed, imageCommitted,
                  appActive, waylandConnected, firstFrame, storageProven>>

Tick ==
    /\ phase \notin Terminal
    /\ now < HardDeadline
    /\ now' = now + 1
    /\ UNCHANGED <<mode, phase, displayProven, snapshotSealed, imageCommitted,
                  appActive, waylandConnected, firstFrame, storageProven>>

Next ==
    \/ CoreServicesReady
    \/ InputPolicyReady
    \/ DisplayProviderReady
    \/ StorageDataPlaneReady
    \/ StorageOnlyDataPlaneReady
    \/ SealExecutableSnapshot
    \/ CommitAndActivateImage
    \/ ConnectWayland
    \/ PresentFirstFrame
    \/ Expire
    \/ Revoke
    \/ Tick

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(Tick)
    /\ WF_vars(Expire)

TypeOK ==
    /\ mode \in Modes
    /\ phase \in {Start, CoreReady, InputReady, SnapshotReady, ImageActive,
                  WaylandReady, Presented, StorageUsable, Failed, Revoked}
    /\ now \in 0..HardDeadline
    /\ displayProven \in BOOLEAN
    /\ snapshotSealed \in BOOLEAN
    /\ imageCommitted \in BOOLEAN
    /\ appActive \in BOOLEAN
    /\ waylandConnected \in BOOLEAN
    /\ firstFrame \in BOOLEAN
    /\ storageProven \in BOOLEAN

DeadlineOrder ==
    /\ CoreDeadline < InputDeadline
    /\ InputDeadline < DisplayDeadline
    /\ InputDeadline < StorageDeadline
    /\ DisplayDeadline < FrameDeadline
    /\ StorageDeadline < SnapshotDeadline
    /\ SnapshotDeadline < FrameDeadline
    /\ FrameDeadline = HardDeadline

PresentedHasCompleteAuthorityChain ==
    firstFrame =>
        /\ mode = Interactive
        /\ phase = Presented
        /\ displayProven
        /\ storageProven
        /\ snapshotSealed
        /\ imageCommitted
        /\ appActive
        /\ waylandConnected

StorageSuccessHasProvenDataPlane ==
    phase = StorageUsable => mode = StorageOnly /\ storageProven

NoPartialImageBecomesActive ==
    (imageCommitted \/ appActive \/ waylandConnected \/ firstFrame) =>
        snapshotSealed /\ storageProven

NoConnectionBeforeActivation ==
    (waylandConnected \/ firstFrame) => imageCommitted /\ appActive

FirstFrameIsTerminal ==
    firstFrame => phase = Presented

EventuallyTerminal == <>(phase \in Terminal)

=============================================================================
