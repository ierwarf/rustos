------------------------- MODULE DvmAtomicScanout --------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Physical AMD zero-copy composition and atomic KMS ownership contract.

Concrete owners:
  driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-display.c
  driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-gpu-runtime.c
  driver-domains/linux/package/rustos-dvm-display/src/rustos_dvm_ivshmem_uio.c

RustOS owns three immutable source slots.  Their complete backing must be
DMA-pinnable and mapped into the VFIO IOAS before the DVM exporter can grant
the AMD GPU read-only DMA-BUF mappings.  The DVM adapter first owns a fixed
ZONE_DEVICE page-map for the complete pixel aperture; raw physical addresses
without live page metadata cannot become DMA-BUF authority.  GLES imports each source as an
EGLImage.  Before GLES samples a source, the kernel validates the exact live
slot/generation invitation and materializes the completed CPU producer release
as a sync_file; EGL imports and server-waits on that exact fence.  GLES then
executes the bounded compositor vocabulary into one of three separate GBM
output buffers and produces a GPU completion fence.  Atomic KMS accepts that
possibly-unsignalled fence as IN_FENCE_FD instead of forcing a CPU pre-wait;
the page flip cannot complete before the fence.  The source is released and
the old output becomes reusable only after the new page-flip fence.  Offline
revokes the whole source pool and publishes no success evidence.

This is deliberately not the old model in which a RustOS source slot itself
became the KMS front buffer.  That would omit the GPU composition ownership
and could falsely call a staged upload zero-copy.
*******************************************************************************)

CONSTANTS Slots, MaxGeneration

SourceFree == "source-free"
SourceReady == "source-ready"
SourceInFlight == "source-in-flight"
NoSlot == 99

Bound == "bound"
PageMapOwned == "page-map-owned"
ExporterOpen == "exporter-open"
SourcesImported == "sources-imported"
KmsReady == "kms-ready"
PeerConfirmed == "peer-confirmed"
RelayReady == "relay-ready"
Offline == "offline"

VARIABLES sourceState,
          sourceGeneration,
          publishedGeneration,
          presentedGeneration,
          releasedGeneration,
          revokedGeneration,
          pendingSource,
          pendingOutput,
          pendingGeneration,
          acquireSyncSlot,
          acquireSyncGeneration,
          acquireSyncIssued,
          acquireSyncConsumed,
          frontOutput,
          gpuFenceComplete,
          pageFlipPending,
          pageFlipComplete,
          dmaReadSlots,
          dmaWriteSlots,
          dmaCoherent,
          vfioBackingPinnable,
          vfioDmaMapped,
          setupPhase,
          evidencePublished,
          online

vars == <<sourceState, sourceGeneration, publishedGeneration,
          presentedGeneration, releasedGeneration, revokedGeneration,
          pendingSource, pendingOutput, pendingGeneration, frontOutput,
          acquireSyncSlot, acquireSyncGeneration, acquireSyncIssued,
          acquireSyncConsumed,
          gpuFenceComplete, pageFlipPending, pageFlipComplete,
          dmaReadSlots, dmaWriteSlots, dmaCoherent, vfioBackingPinnable,
          vfioDmaMapped, setupPhase,
          evidencePublished, online>>

FreeSources == {s \in Slots : sourceState[s] = SourceFree}
ReadySources == {s \in Slots : sourceState[s] = SourceReady}
InFlightSources == {s \in Slots : sourceState[s] = SourceInFlight}

Init ==
    /\ sourceState = [s \in Slots |-> SourceFree]
    /\ sourceGeneration = [s \in Slots |-> 0]
    /\ publishedGeneration = 0
    /\ presentedGeneration = 0
    /\ releasedGeneration = [s \in Slots |-> 0]
    /\ revokedGeneration = [s \in Slots |-> 0]
    /\ pendingSource = NoSlot
    /\ pendingOutput = NoSlot
    /\ pendingGeneration = 0
    /\ acquireSyncSlot = NoSlot
    /\ acquireSyncGeneration = 0
    /\ acquireSyncIssued = FALSE
    /\ acquireSyncConsumed = FALSE
    /\ frontOutput = NoSlot
    /\ gpuFenceComplete = FALSE
    /\ pageFlipPending = FALSE
    /\ pageFlipComplete = FALSE
    /\ dmaReadSlots = {}
    /\ dmaWriteSlots = {}
    /\ dmaCoherent \in BOOLEAN
    /\ vfioBackingPinnable \in BOOLEAN
    /\ vfioDmaMapped = FALSE
    /\ setupPhase = Bound
    /\ evidencePublished = FALSE
    /\ online = TRUE

MapVfioPixelBacking ==
    /\ online
    /\ setupPhase = Bound
    /\ vfioBackingPinnable
    /\ ~vfioDmaMapped
    /\ vfioDmaMapped' = TRUE
    /\ UNCHANGED <<sourceState, sourceGeneration, publishedGeneration,
                  presentedGeneration, releasedGeneration, revokedGeneration,
                  pendingSource, pendingOutput, pendingGeneration, frontOutput,
                  acquireSyncSlot, acquireSyncGeneration, acquireSyncIssued,
                  acquireSyncConsumed,
                  gpuFenceComplete, pageFlipPending, pageFlipComplete,
                  dmaReadSlots, dmaWriteSlots, dmaCoherent,
                  vfioBackingPinnable, setupPhase, evidencePublished, online>>

OwnSourcePageMap ==
    /\ online
    /\ setupPhase = Bound
    /\ vfioDmaMapped
    /\ setupPhase' = PageMapOwned
    /\ UNCHANGED <<sourceState, sourceGeneration, publishedGeneration,
                  presentedGeneration, releasedGeneration, revokedGeneration,
                  pendingSource, pendingOutput, pendingGeneration, frontOutput,
                  acquireSyncSlot, acquireSyncGeneration, acquireSyncIssued,
                  acquireSyncConsumed,
                  gpuFenceComplete, pageFlipPending, pageFlipComplete,
                  dmaReadSlots, dmaWriteSlots, dmaCoherent,
                  vfioBackingPinnable, vfioDmaMapped, evidencePublished,
                  online>>

OpenExporter ==
    /\ online
    /\ setupPhase = PageMapOwned
    /\ vfioDmaMapped
    /\ setupPhase' = ExporterOpen
    /\ UNCHANGED <<sourceState, sourceGeneration, publishedGeneration,
                  presentedGeneration, releasedGeneration, revokedGeneration,
                  pendingSource, pendingOutput, pendingGeneration, frontOutput,
                  acquireSyncSlot, acquireSyncGeneration, acquireSyncIssued,
                  acquireSyncConsumed,
                  gpuFenceComplete, pageFlipPending, pageFlipComplete,
                  dmaReadSlots, dmaWriteSlots, dmaCoherent,
                  vfioBackingPinnable, vfioDmaMapped, evidencePublished,
                  online>>

ImportReadOnlySources ==
    /\ online
    /\ setupPhase = ExporterOpen
    /\ dmaCoherent
    /\ setupPhase' = SourcesImported
    /\ dmaReadSlots' = Slots
    /\ UNCHANGED <<sourceState, sourceGeneration, publishedGeneration,
                  presentedGeneration, releasedGeneration, revokedGeneration,
                  pendingSource, pendingOutput, pendingGeneration, frontOutput,
                  acquireSyncSlot, acquireSyncGeneration, acquireSyncIssued,
                  acquireSyncConsumed,
                  gpuFenceComplete, pageFlipPending, pageFlipComplete,
                  dmaWriteSlots, dmaCoherent, vfioBackingPinnable,
                  vfioDmaMapped, evidencePublished, online>>

CompleteKmsSetup ==
    /\ online
    /\ setupPhase = SourcesImported
    /\ setupPhase' = KmsReady
    /\ UNCHANGED <<sourceState, sourceGeneration, publishedGeneration,
                  presentedGeneration, releasedGeneration, revokedGeneration,
                  pendingSource, pendingOutput, pendingGeneration, frontOutput,
                  acquireSyncSlot, acquireSyncGeneration, acquireSyncIssued,
                  acquireSyncConsumed,
                  gpuFenceComplete, pageFlipPending, pageFlipComplete,
                  dmaReadSlots, dmaWriteSlots, dmaCoherent,
                  vfioBackingPinnable, vfioDmaMapped, evidencePublished,
                  online>>

PublishSource(s) ==
    /\ online
    /\ s \in FreeSources
    /\ publishedGeneration <= MaxGeneration - 2
    /\ sourceState' = [sourceState EXCEPT ![s] = SourceReady]
    /\ sourceGeneration' =
         [sourceGeneration EXCEPT ![s] = publishedGeneration + 2]
    /\ publishedGeneration' = publishedGeneration + 2
    /\ UNCHANGED <<presentedGeneration, releasedGeneration, revokedGeneration,
                  pendingSource, pendingOutput, pendingGeneration, frontOutput,
                  acquireSyncSlot, acquireSyncGeneration, acquireSyncIssued,
                  acquireSyncConsumed,
                  gpuFenceComplete, pageFlipPending, pageFlipComplete,
                  dmaReadSlots, dmaWriteSlots, dmaCoherent,
                  vfioBackingPinnable, vfioDmaMapped, setupPhase,
                  evidencePublished, online>>

ConfirmPeer ==
    /\ online
    /\ setupPhase = KmsReady
    /\ ReadySources # {}
    /\ setupPhase' = PeerConfirmed
    /\ UNCHANGED <<sourceState, sourceGeneration, publishedGeneration,
                  presentedGeneration, releasedGeneration, revokedGeneration,
                  pendingSource, pendingOutput, pendingGeneration, frontOutput,
                  acquireSyncSlot, acquireSyncGeneration, acquireSyncIssued,
                  acquireSyncConsumed,
                  gpuFenceComplete, pageFlipPending, pageFlipComplete,
                  dmaReadSlots, dmaWriteSlots, dmaCoherent,
                  vfioBackingPinnable, vfioDmaMapped, evidencePublished,
                  online>>

IssueAcquireSyncFile(s) ==
    /\ online
    /\ setupPhase \in {PeerConfirmed, RelayReady}
    /\ s \in ReadySources
    /\ sourceGeneration[s] > presentedGeneration
    /\ \A ready \in ReadySources :
           sourceGeneration[s] <= sourceGeneration[ready]
    /\ pendingSource = NoSlot
    /\ ~pageFlipPending
    /\ ~acquireSyncIssued
    /\ acquireSyncSlot' = s
    /\ acquireSyncGeneration' = sourceGeneration[s]
    /\ acquireSyncIssued' = TRUE
    /\ acquireSyncConsumed' = FALSE
    /\ UNCHANGED <<sourceState, sourceGeneration, publishedGeneration,
                  presentedGeneration, releasedGeneration, revokedGeneration,
                  pendingSource, pendingOutput, pendingGeneration, frontOutput,
                  gpuFenceComplete, pageFlipPending, pageFlipComplete,
                  dmaReadSlots, dmaWriteSlots, dmaCoherent,
                  vfioBackingPinnable, vfioDmaMapped, setupPhase,
                  evidencePublished, online>>

BeginGpuComposition(s, output) ==
    /\ online
    /\ setupPhase \in {PeerConfirmed, RelayReady}
    /\ s \in ReadySources
    /\ sourceGeneration[s] > presentedGeneration
    /\ \A ready \in ReadySources :
           sourceGeneration[s] <= sourceGeneration[ready]
    /\ output \in Slots
    /\ output # frontOutput
    /\ pendingSource = NoSlot
    /\ ~pageFlipPending
    /\ acquireSyncIssued
    /\ ~acquireSyncConsumed
    /\ acquireSyncSlot = s
    /\ acquireSyncGeneration = sourceGeneration[s]
    /\ sourceState' = [sourceState EXCEPT ![s] = SourceInFlight]
    /\ pendingSource' = s
    /\ pendingOutput' = output
    /\ pendingGeneration' = sourceGeneration[s]
    /\ gpuFenceComplete' = FALSE
    /\ pageFlipComplete' = FALSE
    /\ acquireSyncConsumed' = TRUE
    /\ UNCHANGED <<sourceGeneration, publishedGeneration, presentedGeneration,
                  releasedGeneration, revokedGeneration, frontOutput,
                  acquireSyncSlot, acquireSyncGeneration, acquireSyncIssued,
                  pageFlipPending, dmaReadSlots, dmaWriteSlots, dmaCoherent,
                  vfioBackingPinnable, vfioDmaMapped, setupPhase,
                  evidencePublished, online>>

CompleteGpuFence ==
    /\ online
    /\ pendingSource \in Slots
    /\ ~gpuFenceComplete
    /\ gpuFenceComplete' = TRUE
    /\ UNCHANGED <<sourceState, sourceGeneration, publishedGeneration,
                  presentedGeneration, releasedGeneration, revokedGeneration,
                  pendingSource, pendingOutput, pendingGeneration, frontOutput,
                  acquireSyncSlot, acquireSyncGeneration, acquireSyncIssued,
                  acquireSyncConsumed,
                  pageFlipPending, pageFlipComplete, dmaReadSlots,
                  dmaWriteSlots, dmaCoherent, vfioBackingPinnable,
                  vfioDmaMapped, setupPhase, evidencePublished, online>>

BeginAtomicPageFlip ==
    /\ online
    /\ pendingSource \in Slots
    /\ pendingOutput \in Slots
    /\ ~pageFlipPending
    /\ pageFlipPending' = TRUE
    /\ pageFlipComplete' = FALSE
    /\ UNCHANGED <<sourceState, sourceGeneration, publishedGeneration,
                  presentedGeneration, releasedGeneration, revokedGeneration,
                  pendingSource, pendingOutput, pendingGeneration, frontOutput,
                  acquireSyncSlot, acquireSyncGeneration, acquireSyncIssued,
                  acquireSyncConsumed,
                  gpuFenceComplete, dmaReadSlots, dmaWriteSlots, dmaCoherent,
                  vfioBackingPinnable, vfioDmaMapped, setupPhase,
                  evidencePublished, online>>

CompletePageFlip ==
    /\ online
    /\ pageFlipPending
    /\ gpuFenceComplete
    /\ ~pageFlipComplete
    /\ pageFlipComplete' = TRUE
    /\ UNCHANGED <<sourceState, sourceGeneration, publishedGeneration,
                  presentedGeneration, releasedGeneration, revokedGeneration,
                  pendingSource, pendingOutput, pendingGeneration, frontOutput,
                  acquireSyncSlot, acquireSyncGeneration, acquireSyncIssued,
                  acquireSyncConsumed,
                  gpuFenceComplete, pageFlipPending, dmaReadSlots,
                  dmaWriteSlots, dmaCoherent, vfioBackingPinnable,
                  vfioDmaMapped, setupPhase, evidencePublished, online>>

CommitPresentedOutput ==
    /\ online
    /\ pageFlipPending
    /\ pageFlipComplete
    /\ pendingSource \in Slots
    /\ sourceState[pendingSource] = SourceInFlight
    /\ sourceGeneration[pendingSource] = pendingGeneration
    /\ sourceState' =
         [sourceState EXCEPT ![pendingSource] = SourceFree]
    /\ releasedGeneration' =
         [releasedGeneration EXCEPT ![pendingSource] = pendingGeneration]
    /\ presentedGeneration' = pendingGeneration
    /\ frontOutput' = pendingOutput
    /\ pendingSource' = NoSlot
    /\ pendingOutput' = NoSlot
    /\ pendingGeneration' = 0
    /\ acquireSyncSlot' = NoSlot
    /\ acquireSyncGeneration' = 0
    /\ acquireSyncIssued' = FALSE
    /\ acquireSyncConsumed' = FALSE
    /\ gpuFenceComplete' = FALSE
    /\ pageFlipPending' = FALSE
    /\ pageFlipComplete' = FALSE
    /\ setupPhase' = RelayReady
    /\ UNCHANGED <<sourceGeneration, publishedGeneration, revokedGeneration,
                  dmaReadSlots, dmaWriteSlots, dmaCoherent,
                  vfioBackingPinnable, vfioDmaMapped, evidencePublished,
                  online>>

PublishPhysicalEvidence ==
    /\ online
    /\ setupPhase = RelayReady
    /\ presentedGeneration > 0
    /\ dmaReadSlots = Slots
    /\ dmaWriteSlots = {}
    /\ evidencePublished' = TRUE
    /\ UNCHANGED <<sourceState, sourceGeneration, publishedGeneration,
                  presentedGeneration, releasedGeneration, revokedGeneration,
                  pendingSource, pendingOutput, pendingGeneration, frontOutput,
                  acquireSyncSlot, acquireSyncGeneration, acquireSyncIssued,
                  acquireSyncConsumed,
                  gpuFenceComplete, pageFlipPending, pageFlipComplete,
                  dmaReadSlots, dmaWriteSlots, dmaCoherent,
                  vfioBackingPinnable, vfioDmaMapped, setupPhase, online>>

GoOffline ==
    /\ online
    /\ online' = FALSE
    /\ sourceState' = [s \in Slots |-> SourceFree]
    /\ revokedGeneration' = [s \in Slots |-> sourceGeneration[s]]
    /\ pendingSource' = NoSlot
    /\ pendingOutput' = NoSlot
    /\ pendingGeneration' = 0
    /\ acquireSyncSlot' = NoSlot
    /\ acquireSyncGeneration' = 0
    /\ acquireSyncIssued' = FALSE
    /\ acquireSyncConsumed' = FALSE
    /\ frontOutput' = NoSlot
    /\ gpuFenceComplete' = FALSE
    /\ pageFlipPending' = FALSE
    /\ pageFlipComplete' = FALSE
    /\ dmaReadSlots' = {}
    /\ dmaWriteSlots' = {}
    /\ vfioDmaMapped' = FALSE
    /\ setupPhase' = Offline
    /\ evidencePublished' = FALSE
    /\ UNCHANGED <<sourceGeneration, publishedGeneration,
                  presentedGeneration, releasedGeneration, dmaCoherent,
                  vfioBackingPinnable>>

Idle == UNCHANGED vars

Next ==
    \/ MapVfioPixelBacking
    \/ OwnSourcePageMap
    \/ OpenExporter
    \/ ImportReadOnlySources
    \/ CompleteKmsSetup
    \/ \E s \in Slots : PublishSource(s)
    \/ ConfirmPeer
    \/ \E s \in Slots : IssueAcquireSyncFile(s)
    \/ \E s \in Slots, output \in Slots : BeginGpuComposition(s, output)
    \/ CompleteGpuFence
    \/ BeginAtomicPageFlip
    \/ CompletePageFlip
    \/ CommitPresentedOutput
    \/ PublishPhysicalEvidence
    \/ GoOffline
    \/ Idle

TypeOK ==
    /\ sourceState \in [Slots -> {SourceFree, SourceReady, SourceInFlight}]
    /\ sourceGeneration \in [Slots -> 0..MaxGeneration]
    /\ publishedGeneration \in 0..MaxGeneration
    /\ presentedGeneration \in 0..MaxGeneration
    /\ releasedGeneration \in [Slots -> 0..MaxGeneration]
    /\ revokedGeneration \in [Slots -> 0..MaxGeneration]
    /\ pendingSource \in Slots \cup {NoSlot}
    /\ pendingOutput \in Slots \cup {NoSlot}
    /\ pendingGeneration \in 0..MaxGeneration
    /\ acquireSyncSlot \in Slots \cup {NoSlot}
    /\ acquireSyncGeneration \in 0..MaxGeneration
    /\ acquireSyncIssued \in BOOLEAN
    /\ acquireSyncConsumed \in BOOLEAN
    /\ frontOutput \in Slots \cup {NoSlot}
    /\ gpuFenceComplete \in BOOLEAN
    /\ pageFlipPending \in BOOLEAN
    /\ pageFlipComplete \in BOOLEAN
    /\ dmaReadSlots \subseteq Slots
    /\ dmaWriteSlots \subseteq Slots
    /\ dmaCoherent \in BOOLEAN
    /\ vfioBackingPinnable \in BOOLEAN
    /\ vfioDmaMapped \in BOOLEAN
    /\ setupPhase \in {Bound, PageMapOwned, ExporterOpen, SourcesImported, KmsReady,
                         PeerConfirmed, RelayReady, Offline}
    /\ evidencePublished \in BOOLEAN
    /\ online \in BOOLEAN

FixedTripleSlots == Cardinality(Slots) = 3

AtMostOneInFlightSource == Cardinality(InFlightSources) <= 1

PendingNamesPinnedSourceAndSeparateOutput ==
    pendingSource \in Slots =>
        /\ sourceState[pendingSource] = SourceInFlight
        /\ sourceGeneration[pendingSource] = pendingGeneration
        /\ pendingOutput \in Slots
        /\ pendingOutput # frontOutput

NoOrphanInFlightSource ==
    /\ (pendingSource = NoSlot => InFlightSources = {})
    /\ (pendingSource \in Slots => InFlightSources = {pendingSource})

AcquireSyncStateIsCanonical ==
    /\ (~acquireSyncIssued =>
            /\ acquireSyncSlot = NoSlot
            /\ acquireSyncGeneration = 0
            /\ ~acquireSyncConsumed)
    /\ (acquireSyncConsumed => acquireSyncIssued)

UnconsumedAcquireNamesExactReadySource ==
    acquireSyncIssued /\ ~acquireSyncConsumed =>
        /\ pendingSource = NoSlot
        /\ acquireSyncSlot \in ReadySources
        /\ sourceGeneration[acquireSyncSlot] = acquireSyncGeneration

GpuCompositionRequiresExternalAcquireSync ==
    pendingSource \in Slots =>
        /\ acquireSyncIssued
        /\ acquireSyncConsumed
        /\ acquireSyncSlot = pendingSource
        /\ acquireSyncGeneration = pendingGeneration

GpuFencePrecedesPageFlipCompletion == pageFlipComplete => gpuFenceComplete

PageFlipCompletionRequiresPendingOutput ==
    pageFlipComplete => pageFlipPending /\ pendingOutput \in Slots

SourceReleaseRequiresPresentedOutput ==
    \A s \in Slots : releasedGeneration[s] <= presentedGeneration

NoReleaseOfCurrentSource ==
    pendingSource \in Slots =>
        releasedGeneration[pendingSource] < sourceGeneration[pendingSource]

NoDeviceWriteAuthority == dmaWriteSlots = {}

ReadAuthorityRequiresCoherentDma == dmaReadSlots # {} => dmaCoherent

DmaAuthorityRequiresPinnableVfioBacking ==
    dmaReadSlots # {} => vfioBackingPinnable /\ vfioDmaMapped

SetupOrderPreservesReadOnlyAuthority ==
    /\ (setupPhase \in {Bound, PageMapOwned, ExporterOpen} => dmaReadSlots = {})
    /\ (setupPhase \in {SourcesImported, KmsReady, PeerConfirmed, RelayReady} =>
            dmaReadSlots = Slots)

RelayReadinessRequiresCompletedGpuAndKms ==
    setupPhase = RelayReady =>
        /\ presentedGeneration > 0
        /\ frontOutput \in Slots
        /\ dmaReadSlots = Slots

PhysicalEvidenceRequiresVfioBacking ==
    evidencePublished => vfioBackingPinnable /\ vfioDmaMapped

PhysicalEvidenceRequiresZeroCopyComposition ==
    evidencePublished =>
        /\ online
        /\ setupPhase = RelayReady
        /\ presentedGeneration > 0
        /\ dmaReadSlots = Slots
        /\ dmaWriteSlots = {}
        /\ frontOutput \in Slots

OfflineRevokesEverySource ==
    ~online =>
        /\ pendingSource = NoSlot
        /\ pendingOutput = NoSlot
        /\ frontOutput = NoSlot
        /\ dmaReadSlots = {}
        /\ setupPhase = Offline
        /\ ~evidencePublished
        /\ \A s \in Slots : revokedGeneration[s] = sourceGeneration[s]

Spec == Init /\ [][Next]_vars
=============================================================================
