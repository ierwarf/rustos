-------------------------- MODULE DvmGpuAdmission --------------------------
EXTENDS Naturals

(*******************************************************************************
Bounded uiserver admission and promotion for the private display-DVM GPU path.

The mandatory DVM topology keeps legacy CPU presentation live while a worker
allocates and maps the private atlas. The UI thread never owns initialization.
Only a current measured prime that exercises a full atlas upload and textured
draw, an exact provider pitch, and a completed first GPU frame can promote the
consumer. Timeout or revoke fails closed; a revoked context must publish a new
representative prime in its new epoch before another promotion.
*******************************************************************************)

CONSTANTS Strides, Width, Height, BytesPerPixel, MaxMappingBytes,
          MaxClock, MaxEpoch, InitBudget, FirstFrameBudget, MaxGpuFrames

NoProvider == "none"
GenericProvider == "generic"
DvmScanoutProvider == "dvm-scanout"
DvmGpuProvider == "dvm-gpu"

Software == "software"
Waiting == "waiting"
Initializing == "initializing"
Armed == "armed"
Active == "active"
Failed == "failed"

NoOwner == "none"
WorkerOwner == "worker"

PackedStride == Width * BytesPerPixel
ValidStride(stride) ==
    /\ stride >= PackedStride
    /\ stride % BytesPerPixel = 0
    /\ stride * Height <= MaxMappingBytes

VARIABLES dvmTopology,
          provider,
          consumer,
          clock,
          generation,
          contextEpoch,
          primeEpoch,
          primeMeasured,
          representativePrime,
          providerStride,
          consumerStride,
          consumerMappingBytes,
          initializationOwner,
          initializationDeadline,
          firstFrameDeadline,
          sceneReady,
          cpuPresentationLive,
          framePermit,
          gpuFrames

vars == <<dvmTopology, provider, consumer, clock, generation, contextEpoch,
          primeEpoch, primeMeasured, representativePrime, providerStride, consumerStride,
          consumerMappingBytes, initializationOwner, initializationDeadline,
          firstFrameDeadline, sceneReady, cpuPresentationLive, framePermit, gpuFrames>>

Init ==
    /\ dvmTopology \in BOOLEAN
    /\ provider = NoProvider
    /\ consumer = IF dvmTopology THEN Waiting ELSE Software
    /\ clock = 0
    /\ generation = 1
    /\ contextEpoch = 1
    /\ primeEpoch = 0
    /\ primeMeasured = FALSE
    /\ representativePrime = FALSE
    /\ providerStride = 0
    /\ consumerStride = 0
    /\ consumerMappingBytes = 0
    /\ initializationOwner = NoOwner
    /\ initializationDeadline = 0
    /\ firstFrameDeadline = 0
    /\ sceneReady = FALSE
    /\ cpuPresentationLive = TRUE
    /\ framePermit = FALSE
    /\ gpuFrames = 0

PublishGeneric ==
    /\ ~dvmTopology
    /\ provider = NoProvider
    /\ provider' = GenericProvider
    /\ UNCHANGED <<dvmTopology, consumer, clock, generation, contextEpoch,
                  primeEpoch, primeMeasured, representativePrime, providerStride, consumerStride,
                  consumerMappingBytes, initializationOwner,
                  initializationDeadline, firstFrameDeadline, sceneReady,
                  cpuPresentationLive, framePermit, gpuFrames>>

PublishDvmScanout ==
    /\ dvmTopology
    /\ provider = NoProvider
    /\ consumer = Waiting
    /\ provider' = DvmScanoutProvider
    /\ UNCHANGED <<dvmTopology, consumer, clock, generation, contextEpoch,
                  primeEpoch, primeMeasured, representativePrime, providerStride, consumerStride,
                  consumerMappingBytes, initializationOwner,
                  initializationDeadline, firstFrameDeadline, sceneReady,
                  cpuPresentationLive, framePermit, gpuFrames>>

PublishDvmGpu(stride, representative) ==
    /\ dvmTopology
    /\ provider = DvmScanoutProvider
    /\ consumer = Waiting
    /\ stride \in Strides
    /\ provider' = DvmGpuProvider
    /\ providerStride' = stride
    /\ primeEpoch' = contextEpoch
    /\ primeMeasured' = TRUE
    /\ representativePrime' = representative
    /\ UNCHANGED <<dvmTopology, consumer, clock, generation, contextEpoch,
                  consumerStride, consumerMappingBytes, initializationOwner,
                  initializationDeadline, firstFrameDeadline, sceneReady,
                  cpuPresentationLive, framePermit, gpuFrames>>

RejectInvalidStride ==
    /\ dvmTopology
    /\ provider = DvmGpuProvider
    /\ consumer = Waiting
    /\ ~ValidStride(providerStride)
    /\ consumer' = Failed
    /\ cpuPresentationLive' = FALSE
    /\ UNCHANGED <<dvmTopology, provider, clock, generation, contextEpoch,
                  primeEpoch, primeMeasured, representativePrime, providerStride, consumerStride,
                  consumerMappingBytes, initializationOwner,
                  initializationDeadline, firstFrameDeadline, sceneReady,
                  framePermit, gpuFrames>>

RejectUnrepresentativePrime ==
    /\ dvmTopology
    /\ provider = DvmGpuProvider
    /\ consumer = Waiting
    /\ primeMeasured
    /\ ~representativePrime
    /\ consumer' = Failed
    /\ cpuPresentationLive' = FALSE
    /\ UNCHANGED <<dvmTopology, provider, clock, generation, contextEpoch,
                  primeEpoch, primeMeasured, representativePrime, providerStride,
                  consumerStride, consumerMappingBytes, initializationOwner,
                  initializationDeadline, firstFrameDeadline, sceneReady,
                  framePermit, gpuFrames>>

BeginInitialization ==
    /\ dvmTopology
    /\ provider = DvmGpuProvider
    /\ consumer = Waiting
    /\ primeMeasured
    /\ representativePrime
    /\ primeEpoch = contextEpoch
    /\ ValidStride(providerStride)
    /\ clock + InitBudget + FirstFrameBudget <= MaxClock
    /\ consumer' = Initializing
    /\ initializationOwner' = WorkerOwner
    /\ initializationDeadline' = clock + InitBudget
    /\ cpuPresentationLive' = TRUE
    /\ UNCHANGED <<dvmTopology, provider, clock, generation, contextEpoch,
                  primeEpoch, primeMeasured, representativePrime, providerStride, consumerStride,
                  consumerMappingBytes, firstFrameDeadline, sceneReady,
                  framePermit, gpuFrames>>

CompleteInitialization ==
    /\ consumer = Initializing
    /\ initializationOwner = WorkerOwner
    /\ clock < initializationDeadline
    /\ consumer' = Armed
    /\ consumerStride' = providerStride
    /\ consumerMappingBytes' = providerStride * Height
    /\ initializationOwner' = NoOwner
    /\ initializationDeadline' = 0
    /\ firstFrameDeadline' = clock + FirstFrameBudget
    /\ UNCHANGED <<dvmTopology, provider, clock, generation, contextEpoch,
                  primeEpoch, primeMeasured, representativePrime, providerStride, sceneReady,
                  cpuPresentationLive, framePermit, gpuFrames>>

InitializationTimeout ==
    /\ consumer = Initializing
    /\ clock >= initializationDeadline
    /\ consumer' = Failed
    /\ initializationOwner' = NoOwner
    /\ initializationDeadline' = 0
    /\ cpuPresentationLive' = FALSE
    /\ UNCHANGED <<dvmTopology, provider, clock, generation, contextEpoch,
                  primeEpoch, primeMeasured, representativePrime, providerStride, consumerStride,
                  consumerMappingBytes, firstFrameDeadline, sceneReady,
                  framePermit, gpuFrames>>

RetainedSceneReady ==
    /\ consumer \in {Waiting, Initializing, Armed}
    /\ ~sceneReady
    /\ sceneReady' = TRUE
    /\ UNCHANGED <<dvmTopology, provider, consumer, clock, generation,
                  contextEpoch, primeEpoch, primeMeasured, representativePrime, providerStride,
                  consumerStride, consumerMappingBytes, initializationOwner,
                  initializationDeadline, firstFrameDeadline,
                  cpuPresentationLive, framePermit, gpuFrames>>

FirstGpuFrame ==
    /\ consumer = Armed
    /\ sceneReady
    /\ clock < firstFrameDeadline
    /\ consumer' = Active
    /\ firstFrameDeadline' = 0
    /\ cpuPresentationLive' = FALSE
    /\ framePermit' = FALSE
    /\ gpuFrames' = 1
    /\ UNCHANGED <<dvmTopology, provider, clock, generation, contextEpoch,
                  primeEpoch, primeMeasured, representativePrime, providerStride, consumerStride,
                  consumerMappingBytes, initializationOwner,
                  initializationDeadline, sceneReady>>

FirstFrameTimeout ==
    /\ consumer = Armed
    /\ clock >= firstFrameDeadline
    /\ consumer' = Failed
    /\ firstFrameDeadline' = 0
    /\ cpuPresentationLive' = FALSE
    /\ UNCHANGED <<dvmTopology, provider, clock, generation, contextEpoch,
                  primeEpoch, primeMeasured, representativePrime, providerStride, consumerStride,
                  consumerMappingBytes, initializationOwner,
                  initializationDeadline, sceneReady, framePermit, gpuFrames>>

PresentGpuFrame ==
    \* One cadence tick grants exactly one non-accumulating submission permit.
    \* Extra ticks do not queue burst credit; a frame consumes the permit.
    /\ consumer = Active
    /\ framePermit
    /\ gpuFrames < MaxGpuFrames
    /\ framePermit' = FALSE
    /\ gpuFrames' = gpuFrames + 1
    /\ UNCHANGED <<dvmTopology, provider, consumer, clock, generation,
                  contextEpoch, primeEpoch, primeMeasured, representativePrime, providerStride,
                  consumerStride, consumerMappingBytes, initializationOwner,
                  initializationDeadline, firstFrameDeadline, sceneReady,
                  cpuPresentationLive>>

Revoke ==
    /\ dvmTopology
    /\ provider \in {DvmScanoutProvider, DvmGpuProvider}
    /\ consumer # Failed
    /\ contextEpoch < MaxEpoch
    /\ provider' = DvmScanoutProvider
    /\ consumer' = Waiting
    /\ generation' = generation + 1
    /\ contextEpoch' = contextEpoch + 1
    /\ primeEpoch' = 0
    /\ primeMeasured' = FALSE
    /\ representativePrime' = FALSE
    /\ providerStride' = 0
    /\ consumerStride' = 0
    /\ consumerMappingBytes' = 0
    /\ initializationOwner' = NoOwner
    /\ initializationDeadline' = 0
    /\ firstFrameDeadline' = 0
    /\ sceneReady' = FALSE
    /\ cpuPresentationLive' = TRUE
    /\ framePermit' = FALSE
    /\ gpuFrames' = 0
    /\ UNCHANGED <<dvmTopology, clock>>

Tick ==
    /\ clock < MaxClock
    /\ clock' = clock + 1
    /\ framePermit' = TRUE
    /\ UNCHANGED <<dvmTopology, provider, consumer, generation, contextEpoch,
                  primeEpoch, primeMeasured, representativePrime, providerStride, consumerStride,
                  consumerMappingBytes, initializationOwner,
                  initializationDeadline, firstFrameDeadline, sceneReady,
                  cpuPresentationLive, gpuFrames>>

Next ==
    \/ PublishGeneric
    \/ PublishDvmScanout
    \/ \E stride \in Strides, representative \in BOOLEAN :
           PublishDvmGpu(stride, representative)
    \/ RejectInvalidStride
    \/ RejectUnrepresentativePrime
    \/ BeginInitialization
    \/ CompleteInitialization
    \/ InitializationTimeout
    \/ RetainedSceneReady
    \/ FirstGpuFrame
    \/ FirstFrameTimeout
    \/ PresentGpuFrame
    \/ Revoke
    \/ Tick

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(Tick)
    /\ WF_vars(CompleteInitialization \/ InitializationTimeout)
    /\ WF_vars(FirstGpuFrame \/ FirstFrameTimeout)

TypeOK ==
    /\ dvmTopology \in BOOLEAN
    /\ provider \in {NoProvider, GenericProvider, DvmScanoutProvider, DvmGpuProvider}
    /\ consumer \in {Software, Waiting, Initializing, Armed, Active, Failed}
    /\ clock \in 0..MaxClock
    /\ generation \in 1..MaxEpoch
    /\ contextEpoch \in 1..MaxEpoch
    /\ primeEpoch \in 0..MaxEpoch
    /\ primeMeasured \in BOOLEAN
    /\ representativePrime \in BOOLEAN
    /\ providerStride \in Strides \cup {0}
    /\ consumerStride \in Strides \cup {0}
    /\ consumerMappingBytes \in 0..MaxMappingBytes
    /\ initializationOwner \in {NoOwner, WorkerOwner}
    /\ initializationDeadline \in 0..MaxClock
    /\ firstFrameDeadline \in 0..MaxClock
    /\ sceneReady \in BOOLEAN
    /\ cpuPresentationLive \in BOOLEAN
    /\ framePermit \in BOOLEAN
    /\ gpuFrames \in 0..MaxGpuFrames

MandatoryDvmNeverUsesSoftwareFallback ==
    dvmTopology => consumer # Software

InitializationNeverBlocksUiThread ==
    (consumer = Initializing) =>
        /\ initializationOwner = WorkerOwner
        /\ cpuPresentationLive

ArmedKeepsCpuPresentationLive ==
    (consumer = Armed) => cpuPresentationLive

ActiveRequiresCurrentMeasuredPrime ==
    (consumer = Active) =>
        /\ provider = DvmGpuProvider
        /\ primeMeasured
        /\ representativePrime
        /\ primeEpoch = contextEpoch

ActiveRequiresExactProviderStride ==
    (consumer \in {Armed, Active}) =>
        /\ ValidStride(providerStride)
        /\ consumerStride = providerStride
        /\ consumerMappingBytes = providerStride * Height

ActiveRequiresCompletedFirstFrame ==
    (consumer = Active) => gpuFrames > 0

WorkerExistsOnlyDuringInitialization ==
    (initializationOwner = WorkerOwner) <=> (consumer = Initializing)

RevokedEpochCannotRemainActive ==
    (~primeMeasured \/ ~representativePrime \/ primeEpoch # contextEpoch) => consumer # Active

InitializationEventuallySettles ==
    [](consumer = Initializing => <>(consumer # Initializing))

FirstFrameEventuallySettles ==
    [](consumer = Armed => <>(consumer # Armed))

=============================================================================
