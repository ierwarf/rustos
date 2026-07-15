---------------------------- MODULE GuiDvmSurface ----------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Commercial GUI-DVM V3 surface-pool model.

This model is the release contract for the implemented `RSGUI002` transport,
not a compatibility model.  L0 fixes exactly three host-owned pixel slots;
RustOS publishes an even generation and a bounded PRESENT descriptor; Linux
may return only a fixed RELEASE descriptor through its validating module.
Every slot is a complete immutable snapshot; the separate
`gui-dvm-pixel-authority` model owns the split UC-control/WB-pixel mapping and
the DVM read-only authority proof.

The deliberately hostile interleavings include: RustOS publishing before the
Linux module exists, three queued host doorbells, DVM restart after every slot
is READY, a stale readiness acknowledgement, a forged release, and a release
whose host acknowledgement is delayed.  No transition permits a guest pointer,
guest-selected vector, a fourth slot, or a hidden unbounded queue.

The model also captures a source-level recovery rule: DVM restart clears the
old readiness confirmation, and a full-pool present re-invites the newest READY
slot exactly once.  The DVM releases superseded READY records one at a time,
using the same acknowledged RELEASE path, so startup bursts cannot permanently
leak two of the three slots.
*******************************************************************************)

CONSTANTS Slots

Free == "free"
Writing == "writing"
Ready == "ready"
SlotStates == {Free, Writing, Ready}

NoSlot == 99
NoControl == "none"
ReadyControl == "ready-control"
ReleaseControl == "release-control"
DisplaySelection == "display"
StaleSelection == "stale"
NoSelection == "none"
SelectionKinds == {NoSelection, DisplaySelection, StaleSelection}

MaxGeneration == 8
MaxControlSequence == 4
MaxDroppedFrames == 4
MaxRejectedControls == 3

VARIABLES slotState,
          slotGeneration,
          dvmReleasedGeneration,
          writingSlot,
          selectedSlot,
          selectedKind,
          publishedGeneration,
          displayedGeneration,
          peerOnline,
          peerReady,
          transportRevoked,
          expectedInvitation,
          dvmLatchedInvitation,
          readyAcknowledgement,
          readinessConfirmation,
          hostToDvmEvent,
          controlKind,
          controlSequence,
          hostAcknowledgedSequence,
          releaseSlot,
          releaseGeneration,
          droppedFrames,
          rejectedControls

vars == <<slotState, slotGeneration, dvmReleasedGeneration, writingSlot,
          selectedSlot, selectedKind, publishedGeneration, displayedGeneration,
          peerOnline, peerReady, transportRevoked, expectedInvitation,
          dvmLatchedInvitation, readyAcknowledgement, readinessConfirmation,
          hostToDvmEvent, controlKind, controlSequence,
          hostAcknowledgedSequence, releaseSlot, releaseGeneration,
          droppedFrames, rejectedControls>>

ReadySlots == {s \in Slots : slotState[s] = Ready}
FreeSlots == {s \in Slots : slotState[s] = Free}

NewestReadyGeneration ==
    IF ReadySlots = {} THEN 0
    ELSE CHOOSE g \in 0..MaxGeneration :
        /\ \E s \in ReadySlots : slotGeneration[s] = g
        /\ \A t \in ReadySlots : slotGeneration[t] <= g

Init ==
    /\ slotState = [s \in Slots |-> Free]
    /\ slotGeneration = [s \in Slots |-> 0]
    /\ dvmReleasedGeneration = [s \in Slots |-> 0]
    /\ writingSlot = NoSlot
    /\ selectedSlot = NoSlot
    /\ selectedKind = NoSelection
    /\ publishedGeneration = 0
    /\ displayedGeneration = 0
    /\ peerOnline = TRUE
    /\ peerReady = FALSE
    /\ transportRevoked = FALSE
    /\ expectedInvitation = 0
    /\ dvmLatchedInvitation = 0
    /\ readyAcknowledgement = 0
    /\ readinessConfirmation = 0
    /\ hostToDvmEvent = FALSE
    /\ controlKind = NoControl
    /\ controlSequence = 0
    /\ hostAcknowledgedSequence = 0
    /\ releaseSlot = NoSlot
    /\ releaseGeneration = 0
    /\ droppedFrames = 0
    /\ rejectedControls = 0

BeginWrite(s) ==
    /\ ~transportRevoked
    /\ writingSlot = NoSlot
    /\ slotState[s] = Free
    /\ publishedGeneration <= MaxGeneration - 2
    /\ slotState' = [slotState EXCEPT ![s] = Writing]
    /\ writingSlot' = s
    /\ UNCHANGED <<slotGeneration, dvmReleasedGeneration, selectedSlot,
                  selectedKind, publishedGeneration, displayedGeneration,
                  peerOnline, peerReady, transportRevoked, expectedInvitation,
                  dvmLatchedInvitation, readyAcknowledgement,
                  readinessConfirmation, hostToDvmEvent, controlKind,
                  controlSequence, hostAcknowledgedSequence, releaseSlot,
                  releaseGeneration, droppedFrames, rejectedControls>>

Publish(s) ==
    /\ ~transportRevoked
    /\ writingSlot = s
    /\ slotState[s] = Writing
    /\ publishedGeneration <= MaxGeneration - 2
    /\ slotState' = [slotState EXCEPT ![s] = Ready]
    /\ slotGeneration' = [slotGeneration EXCEPT ![s] = publishedGeneration + 2]
    /\ writingSlot' = NoSlot
    /\ publishedGeneration' = publishedGeneration + 2
    /\ expectedInvitation' =
        IF peerReady THEN expectedInvitation ELSE publishedGeneration + 2
    /\ hostToDvmEvent' = TRUE
    /\ UNCHANGED <<dvmReleasedGeneration, selectedSlot, selectedKind,
                  displayedGeneration, peerOnline, peerReady, transportRevoked,
                  dvmLatchedInvitation, readyAcknowledgement,
                  readinessConfirmation, controlKind, controlSequence,
                  hostAcknowledgedSequence, releaseSlot, releaseGeneration,
                  droppedFrames, rejectedControls>>

HostBackpressure ==
    /\ ~transportRevoked
    /\ writingSlot = NoSlot
    /\ FreeSlots = {}
    /\ NewestReadyGeneration # 0
    /\ expectedInvitation' =
        IF ~peerReady /\ expectedInvitation # NewestReadyGeneration
           THEN NewestReadyGeneration
           ELSE expectedInvitation
    /\ hostToDvmEvent' = (hostToDvmEvent \/
        (~peerReady /\ expectedInvitation # NewestReadyGeneration))
    /\ droppedFrames' =
        IF droppedFrames < MaxDroppedFrames THEN droppedFrames + 1 ELSE MaxDroppedFrames
    /\ UNCHANGED <<slotState, slotGeneration, dvmReleasedGeneration,
                  writingSlot, selectedSlot, selectedKind, publishedGeneration,
                  displayedGeneration, peerOnline, peerReady, transportRevoked,
                  dvmLatchedInvitation, readyAcknowledgement,
                  readinessConfirmation, controlKind, controlSequence,
                  hostAcknowledgedSequence, releaseSlot, releaseGeneration,
                  rejectedControls>>

(* The Linux module reconstructs this from BAR2 at probe, even if the earlier
   MSI-X edge occurred before the module or relay existed. *)
DvmLatchInvitation ==
    /\ peerOnline
    /\ ~transportRevoked
    /\ ~peerReady
    /\ expectedInvitation # 0
    /\ dvmLatchedInvitation # expectedInvitation
    /\ dvmLatchedInvitation' = expectedInvitation
    /\ UNCHANGED <<slotState, slotGeneration, dvmReleasedGeneration,
                  writingSlot, selectedSlot, selectedKind, publishedGeneration,
                  displayedGeneration, peerOnline, peerReady, transportRevoked,
                  expectedInvitation, readyAcknowledgement,
                  readinessConfirmation, hostToDvmEvent, controlKind,
                  controlSequence, hostAcknowledgedSequence, releaseSlot,
                  releaseGeneration, droppedFrames, rejectedControls>>

DvmSendReady ==
    /\ peerOnline
    /\ ~transportRevoked
    /\ ~peerReady
    /\ controlKind = NoControl
    /\ expectedInvitation # 0
    /\ dvmLatchedInvitation = expectedInvitation
    /\ readyAcknowledgement # expectedInvitation
    /\ readyAcknowledgement' = expectedInvitation
    /\ controlKind' = ReadyControl
    /\ UNCHANGED <<slotState, slotGeneration, dvmReleasedGeneration,
                  writingSlot, selectedSlot, selectedKind, publishedGeneration,
                  displayedGeneration, peerOnline, peerReady, transportRevoked,
                  expectedInvitation, dvmLatchedInvitation,
                  readinessConfirmation, hostToDvmEvent, controlSequence,
                  hostAcknowledgedSequence, releaseSlot, releaseGeneration,
                  droppedFrames, rejectedControls>>

HostDrainReady ==
    /\ controlKind = ReadyControl
    /\ controlKind' = NoControl
    /\ IF expectedInvitation # 0 /\ readyAcknowledgement = expectedInvitation THEN
          /\ peerReady' = TRUE
          /\ readinessConfirmation' = expectedInvitation
          /\ hostToDvmEvent' = TRUE
          /\ rejectedControls' = rejectedControls
       ELSE
          /\ peerReady' = FALSE
          /\ readinessConfirmation' = 0
          /\ hostToDvmEvent' = hostToDvmEvent
          /\ rejectedControls' =
              IF rejectedControls < MaxRejectedControls
                 THEN rejectedControls + 1 ELSE MaxRejectedControls
    /\ UNCHANGED <<slotState, slotGeneration, dvmReleasedGeneration,
                  writingSlot, selectedSlot, selectedKind, publishedGeneration,
                  displayedGeneration, peerOnline, transportRevoked,
                  expectedInvitation, dvmLatchedInvitation,
                  readyAcknowledgement, controlSequence,
                  hostAcknowledgedSequence, releaseSlot, releaseGeneration,
                  droppedFrames>>

DvmSelectNewest(s) ==
    /\ peerOnline
    /\ peerReady
    /\ ~transportRevoked
    /\ hostToDvmEvent
    /\ selectedSlot = NoSlot
    /\ slotState[s] = Ready
    /\ slotGeneration[s] > dvmReleasedGeneration[s]
    /\ slotGeneration[s] > displayedGeneration
    /\ \A t \in ReadySlots : slotGeneration[t] <= slotGeneration[s]
    /\ selectedSlot' = s
    /\ selectedKind' = DisplaySelection
    /\ hostToDvmEvent' = FALSE
    /\ UNCHANGED <<slotState, slotGeneration, dvmReleasedGeneration,
                  writingSlot, publishedGeneration, displayedGeneration,
                  peerOnline, peerReady, transportRevoked, expectedInvitation,
                  dvmLatchedInvitation, readyAcknowledgement,
                  readinessConfirmation, controlKind, controlSequence,
                  hostAcknowledgedSequence, releaseSlot, releaseGeneration,
                  droppedFrames, rejectedControls>>

DvmSelectStale(s) ==
    /\ peerOnline
    /\ peerReady
    /\ ~transportRevoked
    /\ hostToDvmEvent
    /\ selectedSlot = NoSlot
    /\ slotState[s] = Ready
    /\ slotGeneration[s] > dvmReleasedGeneration[s]
    /\ slotGeneration[s] <= displayedGeneration
    /\ \A t \in ReadySlots : slotGeneration[t] <= displayedGeneration
    /\ selectedSlot' = s
    /\ selectedKind' = StaleSelection
    /\ hostToDvmEvent' = FALSE
    /\ UNCHANGED <<slotState, slotGeneration, dvmReleasedGeneration,
                  writingSlot, publishedGeneration, displayedGeneration,
                  peerOnline, peerReady, transportRevoked, expectedInvitation,
                  dvmLatchedInvitation, readyAcknowledgement,
                  readinessConfirmation, controlKind, controlSequence,
                  hostAcknowledgedSequence, releaseSlot, releaseGeneration,
                  droppedFrames, rejectedControls>>

DvmSubmitRelease ==
    /\ peerOnline
    /\ peerReady
    /\ ~transportRevoked
    /\ selectedSlot \in Slots
    /\ controlKind = NoControl
    /\ controlSequence = hostAcknowledgedSequence
    /\ controlSequence < MaxControlSequence
    /\ slotState[selectedSlot] = Ready
    /\ slotGeneration[selectedSlot] > dvmReleasedGeneration[selectedSlot]
    /\ dvmReleasedGeneration' =
        [dvmReleasedGeneration EXCEPT ![selectedSlot] = slotGeneration[selectedSlot]]
    /\ displayedGeneration' =
        IF selectedKind = DisplaySelection
           THEN slotGeneration[selectedSlot]
           ELSE displayedGeneration
    /\ controlKind' = ReleaseControl
    /\ controlSequence' = controlSequence + 1
    /\ releaseSlot' = selectedSlot
    /\ releaseGeneration' = slotGeneration[selectedSlot]
    /\ selectedSlot' = NoSlot
    /\ selectedKind' = NoSelection
    /\ UNCHANGED <<slotState, slotGeneration, writingSlot, publishedGeneration,
                  peerOnline, peerReady, transportRevoked, expectedInvitation,
                  dvmLatchedInvitation, readyAcknowledgement,
                  readinessConfirmation, hostToDvmEvent,
                  hostAcknowledgedSequence, droppedFrames, rejectedControls>>

HostDrainRelease ==
    /\ controlKind = ReleaseControl
    /\ controlKind' = NoControl
    /\ IF controlSequence = hostAcknowledgedSequence + 1
          /\ releaseSlot \in Slots
          /\ slotState[releaseSlot] = Ready
          /\ slotGeneration[releaseSlot] = releaseGeneration
       THEN
          /\ slotState' = [slotState EXCEPT ![releaseSlot] = Free]
          /\ hostAcknowledgedSequence' = controlSequence
          /\ rejectedControls' = rejectedControls
          /\ transportRevoked' = transportRevoked
          /\ peerReady' = peerReady
          /\ expectedInvitation' = expectedInvitation
          /\ readinessConfirmation' = readinessConfirmation
       ELSE
          /\ UNCHANGED slotState
          /\ hostAcknowledgedSequence' = controlSequence
          /\ rejectedControls' =
              IF rejectedControls < MaxRejectedControls
                 THEN rejectedControls + 1 ELSE MaxRejectedControls
          /\ transportRevoked' = TRUE
          /\ peerReady' = FALSE
          /\ expectedInvitation' = 0
          /\ readinessConfirmation' = 0
    /\ UNCHANGED <<slotGeneration, dvmReleasedGeneration, writingSlot,
                  selectedSlot, selectedKind, publishedGeneration,
                  displayedGeneration, peerOnline, dvmLatchedInvitation,
                  readyAcknowledgement, hostToDvmEvent, controlSequence,
                  releaseSlot, releaseGeneration, droppedFrames>>

(* The validating DVM module rejects malformed or out-of-window RELEASE
   records before they can alter the shared host-control record. *)
DvmRejectForgedRelease ==
    /\ peerOnline
    /\ ~transportRevoked
    /\ rejectedControls' =
        IF rejectedControls < MaxRejectedControls
           THEN rejectedControls + 1 ELSE MaxRejectedControls
    /\ UNCHANGED <<slotState, slotGeneration, dvmReleasedGeneration,
                  writingSlot, selectedSlot, selectedKind, publishedGeneration,
                  displayedGeneration, peerOnline, peerReady, transportRevoked,
                  expectedInvitation, dvmLatchedInvitation,
                  readyAcknowledgement, readinessConfirmation, hostToDvmEvent,
                  controlKind, controlSequence, hostAcknowledgedSequence,
                  releaseSlot, releaseGeneration, droppedFrames>>

DvmOffline ==
    /\ peerOnline
    /\ peerOnline' = FALSE
    /\ peerReady' = FALSE
    /\ expectedInvitation' = 0
    /\ dvmLatchedInvitation' = 0
    /\ readyAcknowledgement' = 0
    /\ readinessConfirmation' = 0
    /\ hostToDvmEvent' = FALSE
    /\ selectedSlot' = NoSlot
    /\ selectedKind' = NoSelection
    /\ controlKind' = NoControl
    /\ UNCHANGED <<slotState, slotGeneration, dvmReleasedGeneration,
                  writingSlot, publishedGeneration, displayedGeneration,
                  transportRevoked, controlSequence,
                  hostAcknowledgedSequence, releaseSlot, releaseGeneration,
                  droppedFrames, rejectedControls>>

DvmRecover ==
    /\ ~peerOnline
    /\ ~transportRevoked
    /\ peerOnline' = TRUE
    /\ UNCHANGED <<slotState, slotGeneration, dvmReleasedGeneration,
                  writingSlot, selectedSlot, selectedKind, publishedGeneration,
                  displayedGeneration, peerReady, transportRevoked,
                  expectedInvitation, dvmLatchedInvitation,
                  readyAcknowledgement, readinessConfirmation, hostToDvmEvent,
                  controlKind, controlSequence, hostAcknowledgedSequence,
                  releaseSlot, releaseGeneration, droppedFrames, rejectedControls>>

Idle == UNCHANGED vars

Next ==
    \/ \E s \in Slots : BeginWrite(s)
    \/ \E s \in Slots : Publish(s)
    \/ HostBackpressure
    \/ DvmLatchInvitation
    \/ DvmSendReady
    \/ HostDrainReady
    \/ \E s \in Slots : DvmSelectNewest(s)
    \/ \E s \in Slots : DvmSelectStale(s)
    \/ DvmSubmitRelease
    \/ HostDrainRelease
    \/ DvmRejectForgedRelease
    \/ DvmOffline
    \/ DvmRecover
    \/ Idle

Spec == Init /\ [][Next]_vars /\ WF_vars(DvmLatchInvitation) /\
        WF_vars(DvmSendReady) /\ WF_vars(HostDrainReady) /\
        WF_vars(DvmSubmitRelease) /\ WF_vars(HostDrainRelease) /\
        WF_vars(HostBackpressure)

TypeOK ==
    /\ slotState \in [Slots -> SlotStates]
    /\ slotGeneration \in [Slots -> 0..MaxGeneration]
    /\ dvmReleasedGeneration \in [Slots -> 0..MaxGeneration]
    /\ writingSlot \in Slots \cup {NoSlot}
    /\ selectedSlot \in Slots \cup {NoSlot}
    /\ selectedKind \in SelectionKinds
    /\ publishedGeneration \in 0..MaxGeneration
    /\ displayedGeneration \in 0..MaxGeneration
    /\ peerOnline \in BOOLEAN
    /\ peerReady \in BOOLEAN
    /\ transportRevoked \in BOOLEAN
    /\ expectedInvitation \in 0..MaxGeneration
    /\ dvmLatchedInvitation \in 0..MaxGeneration
    /\ readyAcknowledgement \in 0..MaxGeneration
    /\ readinessConfirmation \in 0..MaxGeneration
    /\ hostToDvmEvent \in BOOLEAN
    /\ controlKind \in {NoControl, ReadyControl, ReleaseControl}
    /\ controlSequence \in 0..MaxControlSequence
    /\ hostAcknowledgedSequence \in 0..MaxControlSequence
    /\ releaseSlot \in Slots \cup {NoSlot}
    /\ releaseGeneration \in 0..MaxGeneration
    /\ droppedFrames \in 0..MaxDroppedFrames
    /\ rejectedControls \in 0..MaxRejectedControls

FixedThreeSlotPool == Cardinality(Slots) = 3

WriterOwnsExactlyOneSlot ==
    /\ writingSlot = NoSlot <=> {s \in Slots : slotState[s] = Writing} = {}
    /\ writingSlot \in Slots => slotState[writingSlot] = Writing

PublishedSlotsHaveUniqueEvenGenerations ==
    /\ \A s \in Slots : slotState[s] = Ready =>
          /\ slotGeneration[s] # 0
          /\ slotGeneration[s] % 2 = 0
    /\ \A s, t \in Slots :
          s # t /\ slotState[s] = Ready /\ slotState[t] = Ready =>
             slotGeneration[s] # slotGeneration[t]

ReleaseNeverNamesUnpublishedSlot ==
    controlKind = ReleaseControl =>
        /\ releaseSlot \in Slots
        /\ releaseGeneration # 0
        /\ releaseGeneration = dvmReleasedGeneration[releaseSlot]

AtMostOneOutstandingDvmControl ==
    /\ hostAcknowledgedSequence <= controlSequence
    /\ controlSequence - hostAcknowledgedSequence \in 0..1

ReadinessIsExactAndFresh ==
    peerReady =>
        /\ ~transportRevoked
        /\ expectedInvitation # 0
        /\ readyAcknowledgement = expectedInvitation
        /\ readinessConfirmation = expectedInvitation

OfflineCannotRetainConfirmation ==
    (~peerOnline \/ ~peerReady) => readinessConfirmation = 0

SelectedSlotIsHostReady ==
    selectedSlot = NoSlot \/
        /\ selectedSlot \in Slots
        /\ slotState[selectedSlot] = Ready
        /\ slotGeneration[selectedSlot] > dvmReleasedGeneration[selectedSlot]

NewestDisplayCannotRegress ==
    displayedGeneration <= publishedGeneration

StaleReleaseCannotAdvanceDisplay ==
    selectedKind = StaleSelection =>
        selectedSlot \in Slots /\ slotGeneration[selectedSlot] <= displayedGeneration

BackpressureDoesNotCreateCapacity ==
    FreeSlots = {} /\ writingSlot = NoSlot => publishedGeneration = MaxGeneration \/
        \E s \in Slots : slotState[s] = Ready

TransportRevocationFailsClosed ==
    transportRevoked =>
        /\ ~peerReady
        /\ expectedInvitation = 0
        /\ readinessConfirmation = 0

PendingControlEventuallySettles ==
    controlKind # NoControl => <>(controlKind = NoControl \/ transportRevoked)

OfflineFullPoolIsReinvited ==
    peerOnline /\ ~peerReady /\ FreeSlots = {} /\ expectedInvitation = 0 =>
        <>(expectedInvitation # 0 \/ ~peerOnline \/ transportRevoked)

=============================================================================
