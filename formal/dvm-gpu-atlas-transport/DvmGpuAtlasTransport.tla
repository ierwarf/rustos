---------------------- MODULE DvmGpuAtlasTransport ----------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Private RustOS UI-atlas to display-DVM ownership contract.

One frame atomically publishes one immutable atlas slot and one fixed command
batch. QEMU may stage the published pixels once into a virtio-GPU texture, but
that mode can never claim zero copy. A registered physical backend may instead
grant device-read DMA-BUF authority. Backend certification is separate from
the common frame mechanism: only a registered backend-class/mode pair enters
the state machine. Both modes release the source after the GPU fence. The old
front output is released only by a later presentation fence.

Concrete owners:
  libs/driver-domain-protocol/src/lib.rs
  services/uiserver/src/gpu_scene.rs
  kernel/io-manager/src/io/dvm_display.rs
  driver-domains/linux/package/rustos-dvm-display
*******************************************************************************)

CONSTANTS Slots, Values, Outputs, MaxEpoch, MaxGeneration

StagedCopy == "staged-copy"
DirectDmaBuf == "dmabuf"
Modes == {StagedCopy, DirectDmaBuf}
LinearArgb8888 == "argb8888-linear-1plane"
VirtualStaged == "virtual-staged"
PhysicalDirect == "physical-direct"
BackendClasses == {VirtualStaged, PhysicalDirect}

NoDamage == "none"
PartialDamage == "partial"
FullDamage == "full"
DamageKinds == {NoDamage, PartialDamage, FullDamage}

Free == "free"
Writing == "writing"
Published == "published"
Acquired == "acquired"
GpuReading == "gpu-reading"
Released == "released"

Idle == "idle"
Queued == "queued"
Executing == "executing"
GpuDone == "gpu-done"
Presented == "presented"
Rejected == "rejected"

OutputFree == "free"
OutputRendering == "rendering"
OutputFront == "front"
NoSlot == 99
NoOutput == 99

LiveBatches == {Queued, Executing, GpuDone}

VARIABLES backendClass,
          mode,
          wireSubmitMode,
          sourceLayout,
          epoch,
          online,
          publishedValue,
          slotState,
          slotEpoch,
          slotGeneration,
          batchState,
          batchSlot,
          batchEpoch,
          batchGeneration,
          batchDamage,
          stagedCopied,
          textureValue,
          gpuFence,
          presentFence,
          outputState,
          outputValue,
          batchOutput,
          frontOutput,
          sourceReadAuthority,
          sourceWriteAuthority,
          zeroCopyEvidence,
          cpuComposedAccepted

vars == <<backendClass, mode, wireSubmitMode, sourceLayout, epoch, online, publishedValue, slotState, slotEpoch,
          slotGeneration, batchState, batchSlot, batchEpoch, batchGeneration,
          batchDamage, stagedCopied, textureValue, gpuFence, presentFence, outputState, outputValue,
          batchOutput, frontOutput, sourceReadAuthority, sourceWriteAuthority,
          zeroCopyEvidence, cpuComposedAccepted>>

Init ==
    /\ backendClass \in BackendClasses
    /\ mode = IF backendClass = VirtualStaged THEN StagedCopy ELSE DirectDmaBuf
    \* The DVM prime completion authenticates the selected source mode. The
    \* host caches that exact value as the immutable submit-record mode.
    /\ wireSubmitMode = mode
    /\ sourceLayout = LinearArgb8888
    /\ epoch = 1
    /\ online = TRUE
    /\ publishedValue = 0
    /\ slotState = [s \in Slots |-> Free]
    /\ slotEpoch = [s \in Slots |-> 0]
    \* Mapping generation identifies the fixed imported atlas pool for this
    \* provider epoch. Per-frame freshness is carried by the batch value.
    /\ slotGeneration = [s \in Slots |-> epoch]
    /\ batchState = [v \in Values |-> Idle]
    /\ batchSlot = [v \in Values |-> NoSlot]
    /\ batchEpoch = [v \in Values |-> 0]
    /\ batchGeneration = [v \in Values |-> 0]
    /\ batchDamage = [v \in Values |-> NoDamage]
    /\ stagedCopied = [v \in Values |-> FALSE]
    /\ textureValue = 0
    /\ gpuFence = [v \in Values |-> FALSE]
    /\ presentFence = [v \in Values |-> FALSE]
    /\ outputState = [o \in Outputs |-> OutputFree]
    /\ outputValue = [o \in Outputs |-> 0]
    /\ batchOutput = [v \in Values |-> NoOutput]
    /\ frontOutput = NoOutput
    /\ sourceReadAuthority = {}
    /\ sourceWriteAuthority = {}
    /\ zeroCopyEvidence = {}
    /\ cpuComposedAccepted = FALSE

BeginWrite(s) ==
    /\ online
    /\ s \in Slots
    /\ slotState[s] = Free
    /\ slotGeneration[s] = epoch
    /\ slotState' = [slotState EXCEPT ![s] = Writing]
    /\ slotEpoch' = [slotEpoch EXCEPT ![s] = epoch]
    /\ UNCHANGED <<backendClass, mode, wireSubmitMode, sourceLayout, epoch, online, publishedValue, slotGeneration,
                  batchState, batchSlot, batchEpoch, batchGeneration,
                  batchDamage, stagedCopied, textureValue, gpuFence,
                  presentFence, outputState,
                  outputValue, batchOutput, frontOutput, sourceReadAuthority,
                  sourceWriteAuthority, zeroCopyEvidence,
                  cpuComposedAccepted>>

PublishBatch(s, v, damage) ==
    /\ online
    /\ wireSubmitMode = mode
    /\ s \in Slots
    /\ v \in Values
    /\ v = publishedValue + 1
    /\ batchState[v] = Idle
    /\ slotState[s] = Writing
    /\ slotEpoch[s] = epoch
    /\ slotGeneration[s] = epoch
    /\ damage \in DamageKinds
    /\ (v = 1 => damage = FullDamage)
    /\ slotState' = [slotState EXCEPT ![s] = Published]
    /\ publishedValue' = v
    /\ batchState' = [batchState EXCEPT ![v] = Queued]
    /\ batchSlot' = [batchSlot EXCEPT ![v] = s]
    /\ batchEpoch' = [batchEpoch EXCEPT ![v] = epoch]
    /\ batchGeneration' =
         [batchGeneration EXCEPT ![v] = slotGeneration[s]]
    /\ batchDamage' = [batchDamage EXCEPT ![v] = damage]
    /\ UNCHANGED <<backendClass, mode, wireSubmitMode, sourceLayout, epoch, online, slotEpoch, slotGeneration, stagedCopied, gpuFence,
                  presentFence, outputState, outputValue, batchOutput,
                  frontOutput, sourceReadAuthority, sourceWriteAuthority,
                  textureValue, zeroCopyEvidence, cpuComposedAccepted>>

AcquireBatch(v) ==
    /\ online
    /\ v \in Values
    /\ batchState[v] = Queued
    /\ batchEpoch[v] = epoch
    /\ batchSlot[v] \in Slots
    /\ slotState[batchSlot[v]] = Published
    /\ slotGeneration[batchSlot[v]] = batchGeneration[v]
    /\ slotState' = [slotState EXCEPT ![batchSlot[v]] = Acquired]
    /\ UNCHANGED <<backendClass, mode, wireSubmitMode, sourceLayout, epoch, online, publishedValue, slotEpoch,
                  slotGeneration, batchState, batchSlot, batchEpoch,
                  batchGeneration, batchDamage, stagedCopied, textureValue,
                  gpuFence, presentFence,
                  outputState, outputValue, batchOutput, frontOutput,
                  sourceReadAuthority, sourceWriteAuthority, zeroCopyEvidence,
                  cpuComposedAccepted>>

StageCopy(v) ==
    /\ online
    /\ mode = StagedCopy
    /\ v \in Values
    /\ batchState[v] = Queued
    /\ batchEpoch[v] = epoch
    /\ batchSlot[v] \in Slots
    /\ slotState[batchSlot[v]] = Acquired
    /\ stagedCopied' = [stagedCopied EXCEPT ![v] = TRUE]
    /\ UNCHANGED <<backendClass, mode, wireSubmitMode, sourceLayout, epoch, online, publishedValue, slotState, slotEpoch,
                  slotGeneration, batchState, batchSlot, batchEpoch,
                  batchGeneration, batchDamage, textureValue, gpuFence,
                  presentFence, outputState,
                  outputValue, batchOutput, frontOutput, sourceReadAuthority,
                  sourceWriteAuthority, zeroCopyEvidence,
                  cpuComposedAccepted>>

BeginGpu(v, o) ==
    /\ online
    /\ v \in Values
    /\ o \in Outputs
    /\ batchState[v] = Queued
    /\ batchEpoch[v] = epoch
    /\ batchSlot[v] \in Slots
    /\ slotState[batchSlot[v]] = Acquired
    /\ slotGeneration[batchSlot[v]] = batchGeneration[v]
    /\ (mode = DirectDmaBuf \/ stagedCopied[v])
    /\ v = textureValue + 1
    /\ outputState[o] = OutputFree
    /\ slotState' = [slotState EXCEPT ![batchSlot[v]] = GpuReading]
    /\ batchState' = [batchState EXCEPT ![v] = Executing]
    /\ outputState' = [outputState EXCEPT ![o] = OutputRendering]
    /\ outputValue' = [outputValue EXCEPT ![o] = v]
    /\ batchOutput' = [batchOutput EXCEPT ![v] = o]
    /\ textureValue' = v
    /\ sourceReadAuthority' = sourceReadAuthority \cup {batchSlot[v]}
    /\ UNCHANGED <<backendClass, mode, wireSubmitMode, sourceLayout, epoch, online, publishedValue, slotEpoch,
                  slotGeneration, batchSlot, batchEpoch, batchGeneration,
                  batchDamage, stagedCopied, gpuFence, presentFence, frontOutput,
                  sourceWriteAuthority, zeroCopyEvidence,
                  cpuComposedAccepted>>

CompleteGpu(v) ==
    /\ online
    /\ v \in Values
    /\ batchState[v] = Executing
    /\ batchEpoch[v] = epoch
    /\ batchSlot[v] \in Slots
    /\ slotState[batchSlot[v]] = GpuReading
    /\ batchState' = [batchState EXCEPT ![v] = GpuDone]
    /\ slotState' = [slotState EXCEPT ![batchSlot[v]] = Released]
    /\ gpuFence' = [gpuFence EXCEPT ![v] = TRUE]
    /\ sourceReadAuthority' = sourceReadAuthority \ {batchSlot[v]}
    /\ UNCHANGED <<backendClass, mode, wireSubmitMode, sourceLayout, epoch, online, publishedValue, slotEpoch,
                  slotGeneration, batchSlot, batchEpoch, batchGeneration,
                  batchDamage, stagedCopied, textureValue, presentFence,
                  outputState, outputValue,
                  batchOutput, frontOutput, sourceWriteAuthority,
                  zeroCopyEvidence, cpuComposedAccepted>>

ReportZeroCopy(v) ==
    /\ online
    /\ mode = DirectDmaBuf
    /\ v \in Values
    /\ batchState[v] \in {GpuDone, Presented}
    /\ gpuFence[v]
    /\ zeroCopyEvidence' = zeroCopyEvidence \cup {v}
    /\ UNCHANGED <<backendClass, mode, wireSubmitMode, sourceLayout, epoch, online, publishedValue, slotState, slotEpoch,
                  slotGeneration, batchState, batchSlot, batchEpoch,
                  batchGeneration, batchDamage, stagedCopied, textureValue,
                  gpuFence, presentFence,
                  outputState, outputValue, batchOutput, frontOutput,
                  sourceReadAuthority, sourceWriteAuthority,
                  cpuComposedAccepted>>

Present(v) ==
    /\ online
    /\ v \in Values
    /\ batchState[v] = GpuDone
    /\ gpuFence[v]
    /\ batchOutput[v] \in Outputs
    /\ outputState[batchOutput[v]] = OutputRendering
    /\ LET target == batchOutput[v] IN
       /\ batchState' = [batchState EXCEPT ![v] = Presented]
       /\ presentFence' = [presentFence EXCEPT ![v] = TRUE]
       /\ outputState' = [o \in Outputs |->
            IF o = target THEN OutputFront
            ELSE IF o = frontOutput THEN OutputFree
            ELSE outputState[o]]
       /\ outputValue' = [o \in Outputs |->
            IF o = target THEN v
            ELSE IF o = frontOutput THEN 0
            ELSE outputValue[o]]
       /\ frontOutput' = target
    /\ UNCHANGED <<backendClass, mode, wireSubmitMode, sourceLayout, epoch, online, publishedValue, slotState, slotEpoch,
                  slotGeneration, batchSlot, batchEpoch, batchGeneration,
                  batchDamage, stagedCopied, textureValue, gpuFence,
                  batchOutput, sourceReadAuthority,
                  sourceWriteAuthority, zeroCopyEvidence,
                  cpuComposedAccepted>>

RecycleAtlas(s) ==
    /\ online
    /\ s \in Slots
    /\ slotState[s] = Released
    /\ \E v \in Values :
         /\ batchSlot[v] = s
         /\ batchGeneration[v] = slotGeneration[s]
         /\ gpuFence[v]
    /\ slotState' = [slotState EXCEPT ![s] = Free]
    /\ UNCHANGED <<backendClass, mode, wireSubmitMode, sourceLayout, epoch, online, publishedValue, slotEpoch,
                  slotGeneration, batchState, batchSlot, batchEpoch,
                  batchGeneration, batchDamage, stagedCopied, textureValue,
                  gpuFence, presentFence,
                  outputState, outputValue, batchOutput, frontOutput,
                  sourceReadAuthority, sourceWriteAuthority, zeroCopyEvidence,
                  cpuComposedAccepted>>

Revoke ==
    /\ online
    /\ online' = FALSE
    /\ slotState' = [s \in Slots |-> Free]
    /\ batchState' = [v \in Values |->
         IF batchState[v] \in LiveBatches THEN Rejected ELSE batchState[v]]
    /\ outputState' = [o \in Outputs |-> OutputFree]
    /\ outputValue' = [o \in Outputs |-> 0]
    /\ frontOutput' = NoOutput
    /\ sourceReadAuthority' = {}
    /\ sourceWriteAuthority' = {}
    /\ UNCHANGED <<backendClass, mode, wireSubmitMode, sourceLayout, epoch, publishedValue, slotEpoch, slotGeneration,
                  batchSlot, batchEpoch, batchGeneration, stagedCopied,
                  batchDamage, textureValue, gpuFence, presentFence,
                  batchOutput, zeroCopyEvidence,
                  cpuComposedAccepted>>

Reset ==
    /\ ~online
    /\ epoch < MaxEpoch
    /\ epoch' = epoch + 1
    /\ online' = TRUE
    /\ publishedValue' = 0
    /\ slotState' = [s \in Slots |-> Free]
    /\ slotEpoch' = [s \in Slots |-> 0]
    /\ slotGeneration' = [s \in Slots |-> epoch + 1]
    /\ batchState' = [v \in Values |-> Idle]
    /\ batchSlot' = [v \in Values |-> NoSlot]
    /\ batchEpoch' = [v \in Values |-> 0]
    /\ batchGeneration' = [v \in Values |-> 0]
    /\ batchDamage' = [v \in Values |-> NoDamage]
    /\ stagedCopied' = [v \in Values |-> FALSE]
    /\ textureValue' = 0
    /\ gpuFence' = [v \in Values |-> FALSE]
    /\ presentFence' = [v \in Values |-> FALSE]
    /\ outputState' = [o \in Outputs |-> OutputFree]
    /\ outputValue' = [o \in Outputs |-> 0]
    /\ batchOutput' = [v \in Values |-> NoOutput]
    /\ frontOutput' = NoOutput
    /\ sourceReadAuthority' = {}
    /\ sourceWriteAuthority' = {}
    /\ zeroCopyEvidence' = {}
    /\ cpuComposedAccepted' = FALSE
    /\ UNCHANGED <<backendClass, mode, wireSubmitMode, sourceLayout>>

Stutter == UNCHANGED vars

Next ==
    \/ \E s \in Slots : BeginWrite(s)
    \/ \E s \in Slots, v \in Values, damage \in DamageKinds :
         PublishBatch(s, v, damage)
    \/ \E v \in Values : AcquireBatch(v)
    \/ \E v \in Values : StageCopy(v)
    \/ \E v \in Values, o \in Outputs : BeginGpu(v, o)
    \/ \E v \in Values : CompleteGpu(v)
    \/ \E v \in Values : ReportZeroCopy(v)
    \/ \E v \in Values : Present(v)
    \/ \E s \in Slots : RecycleAtlas(s)
    \/ Revoke
    \/ Reset
    \/ Stutter

TypeOK ==
    /\ backendClass \in BackendClasses
    /\ mode \in Modes
    /\ wireSubmitMode \in Modes
    /\ sourceLayout = LinearArgb8888
    /\ epoch \in 1..MaxEpoch
    /\ online \in BOOLEAN
    /\ publishedValue \in 0..Cardinality(Values)
    /\ slotState \in [Slots -> {Free, Writing, Published, Acquired,
                                 GpuReading, Released}]
    /\ slotEpoch \in [Slots -> 0..MaxEpoch]
    /\ slotGeneration \in [Slots -> 0..MaxGeneration]
    /\ batchState \in [Values -> {Idle, Queued, Executing, GpuDone,
                                   Presented, Rejected}]
    /\ batchSlot \in [Values -> Slots \cup {NoSlot}]
    /\ batchEpoch \in [Values -> 0..MaxEpoch]
    /\ batchGeneration \in [Values -> 0..MaxGeneration]
    /\ batchDamage \in [Values -> DamageKinds]
    /\ stagedCopied \in [Values -> BOOLEAN]
    /\ textureValue \in 0..Cardinality(Values)
    /\ gpuFence \in [Values -> BOOLEAN]
    /\ presentFence \in [Values -> BOOLEAN]
    /\ outputState \in [Outputs -> {OutputFree, OutputRendering, OutputFront}]
    /\ outputValue \in [Outputs -> 0..Cardinality(Values)]
    /\ batchOutput \in [Values -> Outputs \cup {NoOutput}]
    /\ frontOutput \in Outputs \cup {NoOutput}
    /\ sourceReadAuthority \subseteq Slots
    /\ sourceWriteAuthority \subseteq Slots
    /\ zeroCopyEvidence \subseteq Values
    /\ cpuComposedAccepted \in BOOLEAN

FixedTripleAtlas == Cardinality(Slots) = 3

AtlasMappingGenerationIsProviderEpoch ==
    \A s \in Slots : slotGeneration[s] = epoch

SubmitModeMatchesAuthenticatedPrime == wireSubmitMode = mode

BackendModeIsRegistered ==
    \/ /\ backendClass = VirtualStaged
       /\ mode = StagedCopy
    \/ /\ backendClass = PhysicalDirect
       /\ mode = DirectDmaBuf

DirectImportUsesExplicitLinearLayout ==
    mode = DirectDmaBuf => sourceLayout = LinearArgb8888

QueuedNamesPublishedAtlas ==
    \A v \in Values : batchState[v] = Queued =>
        /\ batchEpoch[v] = epoch
        /\ batchSlot[v] \in Slots
        /\ slotState[batchSlot[v]] \in {Published, Acquired}
        /\ slotGeneration[batchSlot[v]] = batchGeneration[v]

ExecutionRequiresAcquire ==
    \A v \in Values : batchState[v] = Executing =>
        /\ batchEpoch[v] = epoch
        /\ batchSlot[v] \in Slots
        /\ slotState[batchSlot[v]] = GpuReading
        /\ batchSlot[v] \in sourceReadAuthority

StagedExecutionRequiresCopy ==
    mode = StagedCopy =>
        \A v \in Values : batchState[v] = Executing => stagedCopied[v]

InitialAtlasIsFullyDefined ==
    batchState[1] # Idle => batchDamage[1] = FullDamage

NoDamageOnlyAfterInitialization ==
    \A v \in Values :
        batchState[v] # Idle /\ batchDamage[v] = NoDamage => v > 1

TextureUpdatesAreOrdered ==
    \A v \in Values : batchState[v] \in {Executing, GpuDone, Presented} =>
        textureValue >= v

GpuFenceReleasesAtlas ==
    /\ \A v \in Values : batchState[v] \in {GpuDone, Presented} => gpuFence[v]
    /\ \A s \in sourceReadAuthority :
        \E v \in Values :
            /\ batchState[v] = Executing
            /\ batchSlot[v] = s
            /\ batchEpoch[v] = epoch
            /\ slotGeneration[s] = batchGeneration[v]

PresentRequiresGpuFence ==
    \A v \in Values : batchState[v] = Presented =>
        gpuFence[v] /\ presentFence[v]

AtMostOneFront ==
    Cardinality({o \in Outputs : outputState[o] = OutputFront}) <= 1

FrontIsPresented ==
    \A o \in Outputs : outputState[o] = OutputFront =>
        /\ outputValue[o] \in Values
        /\ batchState[outputValue[o]] = Presented
        /\ presentFence[outputValue[o]]

StagedNeverClaimsZeroCopy ==
    mode = StagedCopy => zeroCopyEvidence = {}

ZeroCopyRequiresCompletedDirectRead ==
    \A v \in zeroCopyEvidence :
        mode = DirectDmaBuf /\ gpuFence[v]

NoDeviceWriteAuthority == sourceWriteAuthority = {}

\* The service mapping is derived from the sealed provider pool, not from an
\* address in the commit request. The mapping may remain pinned after revoke,
\* but the state machine removes publish/read authority and a reset changes
\* the generation before any slot can be submitted again.
ServiceMappedSlots == Slots

ExactServiceSlotCapabilities ==
    ServiceMappedSlots = Slots

NoCpuComposedGpuSuccess == ~cpuComposedAccepted

OfflineRetainsNoAuthority ==
    ~online => sourceReadAuthority = {} /\ sourceWriteAuthority = {}

Spec == Init /\ [][Next]_vars

=============================================================================
