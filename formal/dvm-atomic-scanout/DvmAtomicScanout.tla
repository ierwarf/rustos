------------------------- MODULE DvmAtomicScanout --------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Atomic GUI-DVM scanout completion contract.

Concrete owner:
  driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-display.c

The RustOS slot is an immutable source snapshot. The DVM may copy it into a
non-front KMS buffer and submit an atomic nonblocking page flip, but cannot
release the source slot at submission time. The page-flip event is the display
completion fence. Before release, the old front buffer must receive the same
generation, leaving a synchronized shadow for the following damage rectangle.

This model intentionally excludes GPU command execution and timing. It proves
the authority/order rule that makes a bounded page-flip event meaningful:
there is no state in which a source slot becomes reusable before the display
has completed it and another local buffer contains that exact generation.
*******************************************************************************)

CONSTANTS Slots, Buffers, MaxGeneration

Free == "free"
Ready == "ready"
NoSlot == 99
NoBuffer == 99

VARIABLES slotState,
          slotGeneration,
          publishedGeneration,
          displayedGeneration,
          fencedGeneration,
          releasedGeneration,
          bufferGeneration,
          frontBuffer,
          pendingSlot,
          pendingBuffer,
          pendingGeneration,
          pageFlipPending,
          pageFlipComplete,
          shadowSynchronized

vars == <<slotState, slotGeneration, publishedGeneration, displayedGeneration,
          fencedGeneration, releasedGeneration, bufferGeneration, frontBuffer,
          pendingSlot, pendingBuffer, pendingGeneration, pageFlipPending,
          pageFlipComplete, shadowSynchronized>>

ReadySlots == {s \in Slots : slotState[s] = Ready}
FreeSlots == {s \in Slots : slotState[s] = Free}
BackBuffers == Buffers \ {frontBuffer}

Init ==
    /\ slotState = [s \in Slots |-> Free]
    /\ slotGeneration = [s \in Slots |-> 0]
    /\ publishedGeneration = 0
    /\ displayedGeneration = 0
    /\ fencedGeneration = 0
    /\ releasedGeneration = [s \in Slots |-> 0]
    /\ bufferGeneration = [b \in Buffers |-> 0]
    /\ frontBuffer \in Buffers
    /\ pendingSlot = NoSlot
    /\ pendingBuffer = NoBuffer
    /\ pendingGeneration = 0
    /\ pageFlipPending = FALSE
    /\ pageFlipComplete = FALSE
    /\ shadowSynchronized = FALSE

Publish(s) ==
    /\ s \in FreeSlots
    /\ publishedGeneration <= MaxGeneration - 2
    /\ slotState' = [slotState EXCEPT ![s] = Ready]
    /\ slotGeneration' = [slotGeneration EXCEPT ![s] = publishedGeneration + 2]
    /\ publishedGeneration' = publishedGeneration + 2
    /\ UNCHANGED <<displayedGeneration, fencedGeneration, releasedGeneration,
                  bufferGeneration, frontBuffer, pendingSlot, pendingBuffer,
                  pendingGeneration, pageFlipPending, pageFlipComplete,
                  shadowSynchronized>>

BeginAtomicFlip(s, b) ==
    /\ s \in ReadySlots
    /\ slotGeneration[s] > displayedGeneration
    /\ ~pageFlipPending
    /\ b \in BackBuffers
    /\ pendingSlot' = s
    /\ pendingBuffer' = b
    /\ pendingGeneration' = slotGeneration[s]
    /\ bufferGeneration' = [bufferGeneration EXCEPT ![b] = slotGeneration[s]]
    /\ pageFlipPending' = TRUE
    /\ pageFlipComplete' = FALSE
    /\ shadowSynchronized' = FALSE
    /\ UNCHANGED <<slotState, slotGeneration, publishedGeneration,
                  displayedGeneration, fencedGeneration, releasedGeneration,
                  frontBuffer>>

CompletePageFlip ==
    /\ pageFlipPending
    /\ ~pageFlipComplete
    /\ pageFlipComplete' = TRUE
    /\ fencedGeneration' = pendingGeneration
    /\ UNCHANGED <<slotState, slotGeneration, publishedGeneration,
                  displayedGeneration, releasedGeneration, bufferGeneration,
                  frontBuffer, pendingSlot, pendingBuffer, pendingGeneration,
                  pageFlipPending, shadowSynchronized>>

SynchronizeShadow ==
    /\ pageFlipPending
    /\ pageFlipComplete
    /\ ~shadowSynchronized
    /\ frontBuffer # pendingBuffer
    /\ bufferGeneration' = [bufferGeneration EXCEPT ![frontBuffer] = pendingGeneration]
    /\ shadowSynchronized' = TRUE
    /\ UNCHANGED <<slotState, slotGeneration, publishedGeneration,
                  displayedGeneration, fencedGeneration, releasedGeneration,
                  frontBuffer, pendingSlot, pendingBuffer, pendingGeneration,
                  pageFlipPending, pageFlipComplete>>

ReleaseAfterFence ==
    /\ pageFlipPending
    /\ pageFlipComplete
    /\ shadowSynchronized
    /\ pendingSlot \in Slots
    /\ slotState[pendingSlot] = Ready
    /\ slotGeneration[pendingSlot] = pendingGeneration
    /\ bufferGeneration[pendingBuffer] = pendingGeneration
    /\ bufferGeneration[frontBuffer] = pendingGeneration
    /\ slotState' = [slotState EXCEPT ![pendingSlot] = Free]
    /\ releasedGeneration' = [releasedGeneration EXCEPT ![pendingSlot] = pendingGeneration]
    /\ displayedGeneration' = pendingGeneration
    /\ frontBuffer' = pendingBuffer
    /\ pendingSlot' = NoSlot
    /\ pendingBuffer' = NoBuffer
    /\ pendingGeneration' = 0
    /\ pageFlipPending' = FALSE
    /\ pageFlipComplete' = FALSE
    /\ shadowSynchronized' = FALSE
    /\ UNCHANGED <<slotGeneration, publishedGeneration, fencedGeneration,
                  bufferGeneration>>

Idle == UNCHANGED vars

Next ==
    \/ \E s \in Slots : Publish(s)
    \/ \E s \in Slots : \E b \in Buffers : BeginAtomicFlip(s, b)
    \/ CompletePageFlip
    \/ SynchronizeShadow
    \/ ReleaseAfterFence
    \/ Idle

TypeOK ==
    /\ slotState \in [Slots -> {Free, Ready}]
    /\ slotGeneration \in [Slots -> 0..MaxGeneration]
    /\ publishedGeneration \in 0..MaxGeneration
    /\ displayedGeneration \in 0..MaxGeneration
    /\ fencedGeneration \in 0..MaxGeneration
    /\ releasedGeneration \in [Slots -> 0..MaxGeneration]
    /\ bufferGeneration \in [Buffers -> 0..MaxGeneration]
    /\ frontBuffer \in Buffers
    /\ pendingSlot \in Slots \cup {NoSlot}
    /\ pendingBuffer \in Buffers \cup {NoBuffer}
    /\ pendingGeneration \in 0..MaxGeneration
    /\ pageFlipPending \in BOOLEAN
    /\ pageFlipComplete \in BOOLEAN
    /\ shadowSynchronized \in BOOLEAN

FixedTripleBuffers == Cardinality(Buffers) = 3

PendingNamesOneImmutableReadySlot ==
    pageFlipPending =>
        /\ pendingSlot \in Slots
        /\ pendingBuffer \in Buffers
        /\ pendingBuffer # frontBuffer
        /\ slotState[pendingSlot] = Ready
        /\ slotGeneration[pendingSlot] = pendingGeneration

FencePrecedesRelease ==
    \A s \in Slots : releasedGeneration[s] <= fencedGeneration

PendingGenerationNeverRegresses ==
    pageFlipPending => pendingGeneration > displayedGeneration

DisplayPrecedesRelease ==
    \A s \in Slots : releasedGeneration[s] <= displayedGeneration

ReleasedDisplayHasShadow ==
    displayedGeneration > 0 /\ ~pageFlipPending =>
        \E b \in Buffers :
            /\ b # frontBuffer
            /\ bufferGeneration[b] = displayedGeneration

NoReleaseBeforeShadow ==
    pageFlipPending /\ pageFlipComplete /\ ~shadowSynchronized =>
        \A s \in Slots : releasedGeneration[s] # pendingGeneration

Spec == Init /\ [][Next]_vars
=============================================================================
