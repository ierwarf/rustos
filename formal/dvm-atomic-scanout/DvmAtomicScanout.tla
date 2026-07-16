------------------------- MODULE DvmAtomicScanout --------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Direct DMA-BUF GUI-DVM scanout ownership contract.

Concrete owners:
  driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-display.c
  driver-domains/linux/package/rustos-dvm-display/src/rustos_dvm_ivshmem_uio.c

The root-only exporter opens and grants device-read mappings before any host
invitation. This breaks the otherwise impossible cycle in which KMS readiness
would require a relay acknowledgement that itself required completed imports.
Relay readiness still requires all three read-only imports, KMS setup, and an
actual published host slot. Device-write authority is never granted.

A page-flip event fences the new front slot. The previous front is released
only after that fence, while the new front remains pinned until a later flip.
An offline transition revokes the entire pool instead of fabricating releases.
*******************************************************************************)

CONSTANTS Slots, MaxGeneration

Free == "free"
Ready == "ready"
Front == "front"
NoSlot == 99

Bound == "bound"
ExporterOpen == "exporter-open"
SlotsImported == "slots-imported"
KmsReady == "kms-ready"
RelayReady == "relay-ready"
Offline == "offline"

VARIABLES slotState,
          slotGeneration,
          publishedGeneration,
          displayedGeneration,
          fencedGeneration,
          releasedGeneration,
          revokedGeneration,
          frontSlot,
          pendingSlot,
          pendingGeneration,
          pageFlipPending,
          pageFlipComplete,
          dmaReadSlots,
          dmaWriteSlots,
          setupPhase,
          online

vars == <<slotState, slotGeneration, publishedGeneration, displayedGeneration,
          fencedGeneration, releasedGeneration, revokedGeneration, frontSlot,
          pendingSlot, pendingGeneration, pageFlipPending, pageFlipComplete,
          dmaReadSlots, dmaWriteSlots, setupPhase, online>>

FreeSlots == {s \in Slots : slotState[s] = Free}
ReadySlots == {s \in Slots : slotState[s] = Ready}
FrontSlots == {s \in Slots : slotState[s] = Front}

Init ==
    /\ slotState = [s \in Slots |-> Free]
    /\ slotGeneration = [s \in Slots |-> 0]
    /\ publishedGeneration = 0
    /\ displayedGeneration = 0
    /\ fencedGeneration = 0
    /\ releasedGeneration = [s \in Slots |-> 0]
    /\ revokedGeneration = [s \in Slots |-> 0]
    /\ frontSlot = NoSlot
    /\ pendingSlot = NoSlot
    /\ pendingGeneration = 0
    /\ pageFlipPending = FALSE
    /\ pageFlipComplete = FALSE
    /\ dmaReadSlots = {}
    /\ dmaWriteSlots = {}
    /\ setupPhase = Bound
    /\ online = TRUE

OpenExporter ==
    /\ online
    /\ setupPhase = Bound
    /\ setupPhase' = ExporterOpen
    /\ UNCHANGED <<slotState, slotGeneration, publishedGeneration,
                  displayedGeneration, fencedGeneration, releasedGeneration,
                  revokedGeneration, frontSlot, pendingSlot, pendingGeneration,
                  pageFlipPending, pageFlipComplete, dmaReadSlots,
                  dmaWriteSlots, online>>

ImportReadOnlySlots ==
    /\ online
    /\ setupPhase = ExporterOpen
    /\ setupPhase' = SlotsImported
    /\ dmaReadSlots' = Slots
    /\ UNCHANGED <<slotState, slotGeneration, publishedGeneration,
                  displayedGeneration, fencedGeneration, releasedGeneration,
                  revokedGeneration, frontSlot, pendingSlot, pendingGeneration,
                  pageFlipPending, pageFlipComplete, dmaWriteSlots, online>>

CompleteKmsSetup ==
    /\ online
    /\ setupPhase = SlotsImported
    /\ setupPhase' = KmsReady
    /\ UNCHANGED <<slotState, slotGeneration, publishedGeneration,
                  displayedGeneration, fencedGeneration, releasedGeneration,
                  revokedGeneration, frontSlot, pendingSlot, pendingGeneration,
                  pageFlipPending, pageFlipComplete, dmaReadSlots,
                  dmaWriteSlots, online>>

AcknowledgeRelay ==
    /\ online
    /\ setupPhase = KmsReady
    /\ ReadySlots # {}
    /\ setupPhase' = RelayReady
    /\ UNCHANGED <<slotState, slotGeneration, publishedGeneration,
                  displayedGeneration, fencedGeneration, releasedGeneration,
                  revokedGeneration, frontSlot, pendingSlot, pendingGeneration,
                  pageFlipPending, pageFlipComplete, dmaReadSlots,
                  dmaWriteSlots, online>>

Publish(s) ==
    /\ online
    /\ s \in FreeSlots
    /\ publishedGeneration <= MaxGeneration - 2
    /\ slotState' = [slotState EXCEPT ![s] = Ready]
    /\ slotGeneration' = [slotGeneration EXCEPT ![s] = publishedGeneration + 2]
    /\ publishedGeneration' = publishedGeneration + 2
    /\ UNCHANGED <<displayedGeneration, fencedGeneration, releasedGeneration,
                  revokedGeneration, frontSlot, pendingSlot, pendingGeneration,
                  pageFlipPending, pageFlipComplete, dmaReadSlots,
                  dmaWriteSlots, setupPhase, online>>

BeginDirectFlip(s) ==
    /\ online
    /\ setupPhase = RelayReady
    /\ s \in ReadySlots
    /\ slotGeneration[s] > displayedGeneration
    /\ ~pageFlipPending
    /\ pendingSlot' = s
    /\ pendingGeneration' = slotGeneration[s]
    /\ pageFlipPending' = TRUE
    /\ pageFlipComplete' = FALSE
    /\ UNCHANGED <<slotState, slotGeneration, publishedGeneration,
                  displayedGeneration, fencedGeneration, releasedGeneration,
                  revokedGeneration, frontSlot, dmaReadSlots, dmaWriteSlots,
                  setupPhase, online>>

CompletePageFlip ==
    /\ online
    /\ pageFlipPending
    /\ ~pageFlipComplete
    /\ pageFlipComplete' = TRUE
    /\ fencedGeneration' = pendingGeneration
    /\ UNCHANGED <<slotState, slotGeneration, publishedGeneration,
                  displayedGeneration, releasedGeneration, revokedGeneration,
                  frontSlot, pendingSlot, pendingGeneration, pageFlipPending,
                  dmaReadSlots, dmaWriteSlots, setupPhase, online>>

CommitPageFlip ==
    /\ online
    /\ pageFlipPending
    /\ pageFlipComplete
    /\ pendingSlot \in Slots
    /\ slotState[pendingSlot] = Ready
    /\ slotGeneration[pendingSlot] = pendingGeneration
    /\ slotState' = [s \in Slots |->
         IF s = pendingSlot THEN Front
         ELSE IF s = frontSlot THEN Free
         ELSE slotState[s]]
    /\ releasedGeneration' =
         IF frontSlot \in Slots
         THEN [releasedGeneration EXCEPT ![frontSlot] = slotGeneration[frontSlot]]
         ELSE releasedGeneration
    /\ displayedGeneration' = pendingGeneration
    /\ frontSlot' = pendingSlot
    /\ pendingSlot' = NoSlot
    /\ pendingGeneration' = 0
    /\ pageFlipPending' = FALSE
    /\ pageFlipComplete' = FALSE
    /\ UNCHANGED <<slotGeneration, publishedGeneration, fencedGeneration,
                  revokedGeneration, dmaReadSlots, dmaWriteSlots, setupPhase,
                  online>>

GoOffline ==
    /\ online
    /\ online' = FALSE
    /\ slotState' = [s \in Slots |-> Free]
    /\ revokedGeneration' = [s \in Slots |-> slotGeneration[s]]
    /\ frontSlot' = NoSlot
    /\ pendingSlot' = NoSlot
    /\ pendingGeneration' = 0
    /\ pageFlipPending' = FALSE
    /\ pageFlipComplete' = FALSE
    /\ dmaReadSlots' = {}
    /\ dmaWriteSlots' = {}
    /\ setupPhase' = Offline
    /\ UNCHANGED <<slotGeneration, publishedGeneration, displayedGeneration,
                  fencedGeneration, releasedGeneration>>

Idle == UNCHANGED vars

Next ==
    \/ OpenExporter
    \/ ImportReadOnlySlots
    \/ CompleteKmsSetup
    \/ AcknowledgeRelay
    \/ \E s \in Slots : Publish(s)
    \/ \E s \in Slots : BeginDirectFlip(s)
    \/ CompletePageFlip
    \/ CommitPageFlip
    \/ GoOffline
    \/ Idle

TypeOK ==
    /\ slotState \in [Slots -> {Free, Ready, Front}]
    /\ slotGeneration \in [Slots -> 0..MaxGeneration]
    /\ publishedGeneration \in 0..MaxGeneration
    /\ displayedGeneration \in 0..MaxGeneration
    /\ fencedGeneration \in 0..MaxGeneration
    /\ releasedGeneration \in [Slots -> 0..MaxGeneration]
    /\ revokedGeneration \in [Slots -> 0..MaxGeneration]
    /\ frontSlot \in Slots \cup {NoSlot}
    /\ pendingSlot \in Slots \cup {NoSlot}
    /\ pendingGeneration \in 0..MaxGeneration
    /\ pageFlipPending \in BOOLEAN
    /\ pageFlipComplete \in BOOLEAN
    /\ dmaReadSlots \subseteq Slots
    /\ dmaWriteSlots \subseteq Slots
    /\ setupPhase \in {Bound, ExporterOpen, SlotsImported, KmsReady,
                         RelayReady, Offline}
    /\ online \in BOOLEAN

FixedTripleSlots == Cardinality(Slots) = 3

AtMostOnePinnedFront == Cardinality(FrontSlots) <= 1

DisplayedSlotRemainsPinned ==
    frontSlot \in Slots =>
        /\ slotState[frontSlot] = Front
        /\ slotGeneration[frontSlot] = displayedGeneration

PendingNamesImmutableReadySlot ==
    pageFlipPending =>
        /\ pendingSlot \in Slots
        /\ slotState[pendingSlot] = Ready
        /\ slotGeneration[pendingSlot] = pendingGeneration
        /\ pendingSlot # frontSlot

FencePrecedesRelease ==
    \A s \in Slots : releasedGeneration[s] <= fencedGeneration

NoReleaseOfCurrentFront ==
    frontSlot \in Slots => releasedGeneration[frontSlot] < slotGeneration[frontSlot]

NoDeviceWriteAuthority == dmaWriteSlots = {}

SetupOrderPreservesReadOnlyAuthority ==
    /\ (setupPhase \in {Bound, ExporterOpen} => dmaReadSlots = {})
    /\ (setupPhase \in {SlotsImported, KmsReady, RelayReady} => dmaReadSlots = Slots)

RelayReadinessRequiresCompleteSetup ==
    setupPhase = RelayReady =>
        /\ dmaReadSlots = Slots
        /\ publishedGeneration > 0

PageFlipRequiresRelayReadiness ==
    pageFlipPending => setupPhase = RelayReady

OfflineRevokesEverySlot ==
    ~online =>
        /\ frontSlot = NoSlot
        /\ pendingSlot = NoSlot
        /\ dmaReadSlots = {}
        /\ setupPhase = Offline
        /\ \A s \in Slots : revokedGeneration[s] = slotGeneration[s]

Spec == Init /\ [][Next]_vars
=============================================================================
