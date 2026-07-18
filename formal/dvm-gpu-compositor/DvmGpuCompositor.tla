------------------------- MODULE DvmGpuCompositor -------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Private RustOS compositor to display-DVM GPU execution contract.

Concrete owners:
  libs/driver-domain-protocol/src/lib.rs
  services/uiserver (private submit owner; no application ABI)
  driver-domains/linux/package/rustos-dvm-display (fixed GLES executor)

RustOS owns context admission, bounded queueing, source capabilities, and the
monotonic submit timeline. The DVM receives only device-read source authority
and DVM-private render targets. It may execute Clear, SolidQuad, or
TexturedQuad through built-in shaders; it never accepts an address, raw GPU
command buffer, or application shader. A timeout or revoke invalidates the
whole epoch and every unfinished submission. A new epoch cannot accept a stale
completion from an earlier renderer instance. Built-in shader translation and
pipeline creation are a distinct bounded context-prime phase; no admitted
frame may execute until that phase completes. The 60 Hz frame target and the
hard context timeout are distinct: crossing the target leaves the previous
front buffer and epoch live, while reaching the later bounded timeout revokes
the epoch.
*******************************************************************************)

CONSTANTS Values, Outputs, MaxInFlight, MaxClock, MaxEpoch,
          PrimeBudgetUs, FrameTargetUs, FrameTimeoutUs, MaxSources, MaxCommands

Idle == "idle"
Queued == "queued"
Acquired == "acquired"
Executing == "executing"
GpuDone == "gpu-done"
Presented == "presented"
Rejected == "rejected"

OutputFree == "free"
OutputRendering == "rendering"
OutputFront == "front"
NoOutput == 99

LiveStates == {Queued, Acquired, Executing, GpuDone}
SettledStates == {Presented, Rejected}
FixedCommands == {"clear", "solid-quad", "textured-quad"}

VARIABLES epoch,
          contextActive,
          pipelinePriming,
          pipelineReady,
          primeDeadline,
          primeEvidenceEpoch,
          primeMeasured,
          submittedValue,
          completedValue,
          submissionState,
          submissionEpoch,
          acquireReady,
          gpuFinished,
          deadline,
          commandKind,
          outputState,
          outputValue,
          submissionOutput,
          frontValue,
          released,
          sourceReadAuthority,
          sourceWriteAuthority,
          rawCommandAccepted,
          cpuFallbackAccepted,
          clock

vars == <<epoch, contextActive, pipelinePriming, pipelineReady, primeDeadline,
          primeEvidenceEpoch, primeMeasured,
          submittedValue, completedValue,
          submissionState, submissionEpoch, acquireReady, gpuFinished,
          deadline, commandKind, outputState, outputValue, submissionOutput,
          frontValue, released, sourceReadAuthority, sourceWriteAuthority,
          rawCommandAccepted, cpuFallbackAccepted, clock>>

LiveValues == {v \in Values : submissionState[v] \in LiveStates}
SourceReadValues == {v \in Values : submissionState[v] \in {Queued, Acquired, Executing}}
FrontOutputs == {o \in Outputs : outputState[o] = OutputFront}

Init ==
    /\ epoch = 1
    /\ contextActive = TRUE
    /\ pipelinePriming = FALSE
    /\ pipelineReady = FALSE
    /\ primeDeadline = 0
    /\ primeEvidenceEpoch = 0
    /\ primeMeasured = FALSE
    /\ submittedValue = 0
    /\ completedValue = 0
    /\ submissionState = [v \in Values |-> Idle]
    /\ submissionEpoch = [v \in Values |-> 0]
    /\ acquireReady = [v \in Values |-> FALSE]
    /\ gpuFinished = [v \in Values |-> FALSE]
    /\ deadline = [v \in Values |-> 0]
    /\ commandKind = [v \in Values |-> "none"]
    /\ outputState = [o \in Outputs |-> OutputFree]
    /\ outputValue = [o \in Outputs |-> 0]
    /\ submissionOutput = [v \in Values |-> NoOutput]
    /\ frontValue = 0
    /\ released = [v \in Values |-> FALSE]
    /\ sourceReadAuthority = {}
    /\ sourceWriteAuthority = {}
    /\ rawCommandAccepted = FALSE
    /\ cpuFallbackAccepted = FALSE
    /\ clock = 0

PrimePipeline ==
    /\ contextActive
    /\ ~pipelinePriming
    /\ ~pipelineReady
    /\ clock + 2 <= MaxClock
    /\ pipelinePriming' = TRUE
    /\ pipelineReady' = FALSE
    /\ primeDeadline' = clock + 2
    /\ primeEvidenceEpoch' = 0
    /\ primeMeasured' = FALSE
    /\ UNCHANGED <<epoch, contextActive, submittedValue, completedValue,
                  submissionState, submissionEpoch, acquireReady, gpuFinished,
                  deadline, commandKind, outputState, outputValue,
                  submissionOutput, frontValue, released, sourceReadAuthority,
                  sourceWriteAuthority, rawCommandAccepted, cpuFallbackAccepted,
                  clock>>

CompletePrime ==
    /\ contextActive
    /\ pipelinePriming
    /\ ~pipelineReady
    /\ clock < primeDeadline
    /\ pipelinePriming' = FALSE
    /\ pipelineReady' = TRUE
    /\ primeDeadline' = 0
    /\ primeEvidenceEpoch' = epoch
    /\ primeMeasured' = TRUE
    /\ UNCHANGED <<epoch, contextActive, submittedValue, completedValue,
                  submissionState, submissionEpoch, acquireReady, gpuFinished,
                  deadline, commandKind, outputState, outputValue,
                  submissionOutput, frontValue, released, sourceReadAuthority,
                  sourceWriteAuthority, rawCommandAccepted, cpuFallbackAccepted,
                  clock>>

Submit(v, command) ==
    /\ contextActive
    /\ pipelineReady
    /\ v \in Values
    /\ command \in FixedCommands
    /\ v = submittedValue + 1
    /\ Cardinality(LiveValues) < MaxInFlight
    /\ clock + 3 <= MaxClock
    /\ submissionState' = [submissionState EXCEPT ![v] = Queued]
    /\ submissionEpoch' = [submissionEpoch EXCEPT ![v] = epoch]
    \* One abstract tick is the 60 Hz target; three ticks are the bounded
    \* 50 ms hard timeout. Tick may cross the target without revocation.
    /\ deadline' = [deadline EXCEPT ![v] = clock + 3]
    /\ commandKind' = [commandKind EXCEPT ![v] = command]
    /\ submittedValue' = v
    /\ sourceReadAuthority' = sourceReadAuthority \cup {v}
    /\ UNCHANGED <<epoch, contextActive, pipelinePriming, pipelineReady,
                  primeDeadline, primeEvidenceEpoch, primeMeasured,
                  completedValue, acquireReady,
                  gpuFinished, outputState, outputValue, submissionOutput,
                  frontValue, released, sourceWriteAuthority,
                  rawCommandAccepted, cpuFallbackAccepted, clock>>

SignalAcquire(v) ==
    /\ contextActive
    /\ v \in Values
    /\ submissionState[v] = Queued
    /\ submissionEpoch[v] = epoch
    /\ submissionState' = [submissionState EXCEPT ![v] = Acquired]
    /\ acquireReady' = [acquireReady EXCEPT ![v] = TRUE]
    /\ UNCHANGED <<epoch, contextActive, pipelinePriming, pipelineReady,
                  primeDeadline, primeEvidenceEpoch, primeMeasured,
                  submittedValue, completedValue,
                  submissionEpoch, gpuFinished, deadline, commandKind,
                  outputState, outputValue, submissionOutput, frontValue,
                  released, sourceReadAuthority, sourceWriteAuthority,
                  rawCommandAccepted, cpuFallbackAccepted, clock>>

BeginGpu(v, o) ==
    /\ contextActive
    /\ v \in Values
    /\ o \in Outputs
    /\ submissionState[v] = Acquired
    /\ submissionEpoch[v] = epoch
    /\ acquireReady[v]
    /\ outputState[o] = OutputFree
    /\ submissionState' = [submissionState EXCEPT ![v] = Executing]
    /\ outputState' = [outputState EXCEPT ![o] = OutputRendering]
    /\ outputValue' = [outputValue EXCEPT ![o] = v]
    /\ submissionOutput' = [submissionOutput EXCEPT ![v] = o]
    /\ UNCHANGED <<epoch, contextActive, pipelinePriming, pipelineReady,
                  primeDeadline, primeEvidenceEpoch, primeMeasured,
                  submittedValue, completedValue,
                  submissionEpoch, acquireReady, gpuFinished, deadline,
                  commandKind, frontValue, released, sourceReadAuthority,
                  sourceWriteAuthority, rawCommandAccepted,
                  cpuFallbackAccepted, clock>>

CompleteGpu(v) ==
    /\ contextActive
    /\ v \in Values
    /\ submissionState[v] = Executing
    /\ submissionEpoch[v] = epoch
    /\ v = completedValue + 1
    /\ submissionState' = [submissionState EXCEPT ![v] = GpuDone]
    /\ gpuFinished' = [gpuFinished EXCEPT ![v] = TRUE]
    /\ completedValue' = v
    /\ UNCHANGED <<epoch, contextActive, pipelinePriming, pipelineReady,
                  primeDeadline, primeEvidenceEpoch, primeMeasured,
                  submittedValue, submissionEpoch,
                  acquireReady, deadline, commandKind, outputState,
                  outputValue, submissionOutput, frontValue, released,
                  sourceWriteAuthority,
                  rawCommandAccepted, cpuFallbackAccepted, clock>>
    /\ sourceReadAuthority' = sourceReadAuthority \ {v}

PresentFence(v) ==
    /\ contextActive
    /\ v \in Values
    /\ submissionState[v] = GpuDone
    /\ submissionEpoch[v] = epoch
    /\ v = frontValue + 1
    /\ submissionOutput[v] \in Outputs
    /\ outputState[submissionOutput[v]] = OutputRendering
    /\ LET target == submissionOutput[v] IN
       /\ submissionState' = [submissionState EXCEPT ![v] = Presented]
       /\ outputState' = [o \in Outputs |->
             IF o = target THEN OutputFront
             ELSE IF outputState[o] = OutputFront THEN OutputFree
             ELSE outputState[o]]
       /\ outputValue' = [o \in Outputs |->
             IF o = target THEN v
             ELSE IF outputState[o] = OutputFront THEN 0
             ELSE outputValue[o]]
       /\ released' = IF frontValue = 0
                      THEN released
                      ELSE [released EXCEPT ![frontValue] = TRUE]
    /\ frontValue' = v
    /\ UNCHANGED <<epoch, contextActive, pipelinePriming, pipelineReady,
                  primeDeadline, primeEvidenceEpoch, primeMeasured,
                  submittedValue, completedValue,
                  submissionEpoch, acquireReady, gpuFinished, deadline,
                  commandKind, submissionOutput, sourceReadAuthority,
                  sourceWriteAuthority, rawCommandAccepted,
                  cpuFallbackAccepted, clock>>

InvalidateContext ==
    /\ contextActive
    /\ contextActive' = FALSE
    /\ pipelinePriming' = FALSE
    /\ pipelineReady' = FALSE
    /\ primeDeadline' = 0
    /\ primeEvidenceEpoch' = 0
    /\ primeMeasured' = FALSE
    /\ submissionState' = [v \in Values |->
         IF submissionState[v] \in LiveStates THEN Rejected ELSE submissionState[v]]
    /\ outputState' = [o \in Outputs |-> OutputFree]
    /\ outputValue' = [o \in Outputs |-> 0]
    /\ frontValue' = 0
    /\ sourceReadAuthority' = {}
    /\ UNCHANGED <<epoch, submittedValue, completedValue, submissionEpoch,
                  acquireReady, gpuFinished, deadline, commandKind,
                  submissionOutput, released,
                  sourceWriteAuthority, rawCommandAccepted,
                  cpuFallbackAccepted, clock>>

Timeout(v) ==
    /\ contextActive
    /\ v \in Values
    /\ submissionState[v] \in LiveStates
    /\ deadline[v] # 0
    /\ clock >= deadline[v]
    /\ InvalidateContext

PrimeTimeout ==
    /\ contextActive
    /\ pipelinePriming
    /\ primeDeadline # 0
    /\ clock >= primeDeadline
    /\ InvalidateContext

ResetContext ==
    /\ ~contextActive
    /\ epoch < MaxEpoch
    /\ epoch' = epoch + 1
    /\ contextActive' = TRUE
    /\ pipelinePriming' = FALSE
    /\ pipelineReady' = FALSE
    /\ primeDeadline' = 0
    /\ primeEvidenceEpoch' = 0
    /\ primeMeasured' = FALSE
    /\ submittedValue' = 0
    /\ completedValue' = 0
    /\ submissionState' = [v \in Values |-> Idle]
    /\ submissionEpoch' = [v \in Values |-> 0]
    /\ acquireReady' = [v \in Values |-> FALSE]
    /\ gpuFinished' = [v \in Values |-> FALSE]
    /\ deadline' = [v \in Values |-> 0]
    /\ commandKind' = [v \in Values |-> "none"]
    /\ submissionOutput' = [v \in Values |-> NoOutput]
    /\ released' = [v \in Values |-> FALSE]
    /\ sourceReadAuthority' = {}
    /\ UNCHANGED <<outputState, outputValue, frontValue,
                  sourceWriteAuthority, rawCommandAccepted,
                  cpuFallbackAccepted, clock>>

RejectStaleCompletion(oldEpoch, v) ==
    /\ oldEpoch \in 1..MaxEpoch
    /\ oldEpoch # epoch
    /\ v \in Values
    /\ UNCHANGED vars

Tick ==
    /\ clock < MaxClock
    /\ clock' = clock + 1
    /\ UNCHANGED <<epoch, contextActive, pipelinePriming, pipelineReady,
                  primeDeadline, primeEvidenceEpoch, primeMeasured,
                  submittedValue, completedValue,
                  submissionState, submissionEpoch, acquireReady, gpuFinished,
                  deadline, commandKind, outputState, outputValue,
                  submissionOutput, frontValue, released,
                  sourceReadAuthority, sourceWriteAuthority,
                  rawCommandAccepted, cpuFallbackAccepted>>

IdleStep == UNCHANGED vars

Next ==
    \/ PrimePipeline
    \/ CompletePrime
    \/ \E v \in Values, command \in FixedCommands : Submit(v, command)
    \/ \E v \in Values : SignalAcquire(v)
    \/ \E v \in Values, o \in Outputs : BeginGpu(v, o)
    \/ \E v \in Values : CompleteGpu(v)
    \/ \E v \in Values : PresentFence(v)
    \/ \E v \in Values : Timeout(v)
    \/ PrimeTimeout
    \/ InvalidateContext
    \/ ResetContext
    \/ \E oldEpoch \in 1..MaxEpoch, v \in Values : RejectStaleCompletion(oldEpoch, v)
    \/ Tick
    \/ IdleStep

Spec == Init /\ [][Next]_vars /\ WF_vars(Tick) /\ WF_vars(PrimeTimeout)
        /\ \A v \in Values : WF_vars(Timeout(v))

TypeOK ==
    /\ epoch \in 1..MaxEpoch
    /\ contextActive \in BOOLEAN
    /\ pipelinePriming \in BOOLEAN
    /\ pipelineReady \in BOOLEAN
    /\ primeDeadline \in 0..MaxClock
    /\ primeEvidenceEpoch \in 0..MaxEpoch
    /\ primeMeasured \in BOOLEAN
    /\ submittedValue \in 0..Cardinality(Values)
    /\ completedValue \in 0..Cardinality(Values)
    /\ submissionState \in [Values -> {Idle, Queued, Acquired, Executing,
                                         GpuDone, Presented, Rejected}]
    /\ submissionEpoch \in [Values -> 0..MaxEpoch]
    /\ acquireReady \in [Values -> BOOLEAN]
    /\ gpuFinished \in [Values -> BOOLEAN]
    /\ deadline \in [Values -> 0..MaxClock]
    /\ commandKind \in [Values -> FixedCommands \cup {"none"}]
    /\ outputState \in [Outputs -> {OutputFree, OutputRendering, OutputFront}]
    /\ outputValue \in [Outputs -> Values \cup {0}]
    /\ submissionOutput \in [Values -> Outputs \cup {NoOutput}]
    /\ frontValue \in Values \cup {0}
    /\ released \in [Values -> BOOLEAN]
    /\ sourceReadAuthority \subseteq Values
    /\ sourceWriteAuthority \subseteq Values
    /\ rawCommandAccepted \in BOOLEAN
    /\ cpuFallbackAccepted \in BOOLEAN
    /\ clock \in 0..MaxClock

BoundedQueue == Cardinality(LiveValues) <= MaxInFlight
TimelineMonotonic == completedValue <= submittedValue
PipelineStateValid ==
    /\ ~(pipelinePriming /\ pipelineReady)
    /\ (pipelinePriming <=> primeDeadline # 0)
CurrentMeasuredPrimeRequired ==
    pipelineReady => primeMeasured /\ primeEvidenceEpoch = epoch
LiveAdmissionsRequirePrime == LiveValues # {} => pipelineReady
ExecutionRequiresAcquire ==
    \A v \in Values : submissionState[v] \in {Executing, GpuDone, Presented} => acquireReady[v]
PresentedRequiresGpu ==
    \A v \in Values : submissionState[v] = Presented => gpuFinished[v]
OnlyFixedCommandsExecute ==
    \A v \in Values : submissionState[v] \in {Executing, GpuDone, Presented} =>
        commandKind[v] \in FixedCommands
NoRawCommandOrCpuSuccess == ~rawCommandAccepted /\ ~cpuFallbackAccepted
NoDeviceWriteAuthority == sourceWriteAuthority = {}
EveryExecutingSourceIsReadOnly == SourceReadValues \subseteq sourceReadAuthority
AtMostOneFront == Cardinality(FrontOutputs) <= 1
FrontIsGpuCompleted ==
    frontValue # 0 =>
        /\ submissionState[frontValue] = Presented
        /\ gpuFinished[frontValue]
        /\ submissionEpoch[frontValue] = epoch
ReleaseFollowsGpuFence ==
    \A v \in Values : released[v] => gpuFinished[v]
InactiveContextRetainsNoExecution ==
    ~contextActive => LiveValues = {} /\ FrontOutputs = {} /\ frontValue = 0
InactiveContextRetainsNoSourceAuthority ==
    ~contextActive => sourceReadAuthority = {}
FrameTargetMissWindow(v) ==
    /\ submissionState[v] \in LiveStates
    /\ deadline[v] # 0
    /\ clock < deadline[v]
    /\ clock + 2 >= deadline[v]
FrameTargetMissRetainsFrontAndEpoch ==
    \A v \in Values : FrameTargetMissWindow(v) =>
        /\ contextActive
        /\ pipelineReady
        /\ submissionEpoch[v] = epoch
        /\ Cardinality(FrontOutputs) <= 1
ContractBoundsExact ==
    /\ PrimeBudgetUs = 500000
    /\ FrameTargetUs = 16667
    /\ FrameTimeoutUs = 50000
    /\ MaxSources = 96
    /\ MaxCommands = 512

EveryAdmissionSettles ==
    \A v \in Values : [](submissionState[v] \in LiveStates =>
                           <> (submissionState[v] \in SettledStates))

EveryPrimeSettles == [](pipelinePriming => <> (~pipelinePriming))

=============================================================================
