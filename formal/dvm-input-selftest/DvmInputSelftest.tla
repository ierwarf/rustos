---------------------------- MODULE DvmInputSelftest --------------------------
EXTENDS Naturals

(*******************************************************************************
Models the KVM-only input self-test in `rustos-dvm-agent`.

The test device is intentionally composite: it must advertise both printable
keyboard capability (so the ordinary keyboard selector opens it) and BTN_LEFT
plus ABS_X/ABS_Y (so the ordinary pointer selector opens a second file
description for the same evdev device).  It emits one non-printable F12
press/release solely to prove keyboard ingress, then emits only absolute
pointer positions.  This rules out the historical failure where pointer events were
silently consumed by the keyboard reader and a long synthetic run flooded a
focused shell with key events.

Concrete owner:
  driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c
  `input_selftest_start`, `input_selftest_emit_cycle`, `stream_input_devices`
  tools/xtask/src/kvm.rs `--exercise-input` readiness markers
*******************************************************************************)

CONSTANTS MotionCycles, MaxClock

Unopened == "unopened"
Streaming == "streaming"
Restoring == "restoring"
Closed == "closed"
Terminated == "terminated"
NormalScheduler == "normal"
BoundedRoundRobin == "bounded-rr"
NoScheduler == "no-process"

VARIABLES pointerCapability,
          vectorReady,
          policyConsumerReady,
          streamState,
          schedulerPolicy,
          rtBudgetBounded,
          admissionPending,
          restoreFailed,
          hardLimitReached,
          keyboardProbes,
          motionCycles,
          pointerPositions,
          clock,
          emitPermit,
          keyboardIngress,
          pointerIngress

vars == <<pointerCapability, vectorReady, policyConsumerReady,
          streamState, schedulerPolicy, rtBudgetBounded,
          admissionPending, restoreFailed, hardLimitReached,
          keyboardProbes, motionCycles,
          pointerPositions, clock, emitPermit, keyboardIngress, pointerIngress>>

Init ==
    /\ pointerCapability = TRUE
    /\ vectorReady = FALSE
    /\ policyConsumerReady = FALSE
    /\ streamState = Unopened
    /\ schedulerPolicy = NormalScheduler
    /\ rtBudgetBounded = FALSE
    /\ admissionPending = FALSE
    /\ restoreFailed = FALSE
    /\ hardLimitReached = FALSE
    /\ keyboardProbes = 0
    /\ motionCycles = 0
    /\ pointerPositions = 0
    /\ clock = 0
    /\ emitPermit = FALSE
    /\ keyboardIngress = FALSE
    /\ pointerIngress = FALSE

ArmReceiverVector ==
    /\ ~vectorReady
    /\ vectorReady' = TRUE
    /\ UNCHANGED <<pointerCapability, policyConsumerReady, streamState,
                  schedulerPolicy, rtBudgetBounded,
                  admissionPending, restoreFailed, hardLimitReached,
                  keyboardProbes, motionCycles, pointerPositions,
                  clock, emitPermit, keyboardIngress, pointerIngress>>

ObservePolicyConsumer ==
    /\ vectorReady
    /\ ~policyConsumerReady
    /\ policyConsumerReady' = TRUE
    /\ UNCHANGED <<pointerCapability, vectorReady, streamState,
                  schedulerPolicy, rtBudgetBounded,
                  admissionPending, restoreFailed, hardLimitReached,
                  keyboardProbes, motionCycles, pointerPositions,
                  clock, emitPermit, keyboardIngress, pointerIngress>>

(*******************************************************************************
The agent may open two independent evdev descriptions only after the
capability probe sees BTN_LEFT plus a usable X/Y axis.  A device that lacks the
pointer capability is not reinterpreted as a keyboard-only success path.
*******************************************************************************)
OpenCompositeStream ==
    /\ streamState = Unopened
    /\ pointerCapability
    /\ vectorReady
    /\ policyConsumerReady
    /\ streamState' = Streaming
    /\ emitPermit' = TRUE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  schedulerPolicy, rtBudgetBounded,
                  admissionPending, restoreFailed, hardLimitReached,
                  keyboardProbes, motionCycles,
                  pointerPositions, clock, keyboardIngress, pointerIngress>>

(*******************************************************************************
The authenticated input stream receives a low SCHED_RR priority only after a
strict RLIMIT_RTTIME ceiling is installed and read back. This bounds guest
scheduling latency while a runaway relay is terminated instead of starving
KMS or recovery indefinitely. Limit installation and SCHED_RR admission are
separate because either syscall or readback may fail. A rollback or final
restore mismatch terminates the agent instead of reconnecting with uncertain
realtime authority.
*******************************************************************************)
InstallInputBudget ==
    /\ streamState = Streaming
    /\ schedulerPolicy = NormalScheduler
    /\ ~rtBudgetBounded
    /\ ~admissionPending
    /\ rtBudgetBounded' = TRUE
    /\ admissionPending' = TRUE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  streamState, schedulerPolicy, restoreFailed,
                  hardLimitReached, keyboardProbes, motionCycles,
                  pointerPositions, clock, emitPermit, keyboardIngress,
                  pointerIngress>>

AdmitBoundedInputScheduler ==
    /\ streamState = Streaming
    /\ schedulerPolicy = NormalScheduler
    /\ rtBudgetBounded
    /\ admissionPending
    /\ schedulerPolicy' = BoundedRoundRobin
    /\ admissionPending' = FALSE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  streamState, rtBudgetBounded, restoreFailed,
                  hardLimitReached, keyboardProbes, motionCycles,
                  pointerPositions, clock, emitPermit, keyboardIngress,
                  pointerIngress>>

RollbackInputAdmission ==
    /\ streamState = Streaming
    /\ schedulerPolicy = NormalScheduler
    /\ rtBudgetBounded
    /\ admissionPending
    /\ streamState' = Closed
    /\ rtBudgetBounded' = FALSE
    /\ admissionPending' = FALSE
    /\ emitPermit' = FALSE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  schedulerPolicy, restoreFailed, hardLimitReached,
                  keyboardProbes, motionCycles, pointerPositions, clock,
                  keyboardIngress, pointerIngress>>

FatalInputAdmissionRollback ==
    /\ streamState = Streaming
    /\ schedulerPolicy = NormalScheduler
    /\ rtBudgetBounded
    /\ admissionPending
    /\ streamState' = Terminated
    /\ schedulerPolicy' = NoScheduler
    /\ rtBudgetBounded' = FALSE
    /\ admissionPending' = FALSE
    /\ restoreFailed' = TRUE
    /\ emitPermit' = FALSE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  hardLimitReached, keyboardProbes, motionCycles,
                  pointerPositions, clock, keyboardIngress, pointerIngress>>

(*******************************************************************************
One F12-only probe establishes keyboard ingress.  It is bounded to one
press/release pair; subsequent self-test work is pointer-only and therefore
cannot turn duration into console command pressure.
*******************************************************************************)
EmitKeyboardProbe ==
    /\ streamState = Streaming
    /\ schedulerPolicy = BoundedRoundRobin
    /\ rtBudgetBounded
    /\ ~admissionPending
    /\ keyboardProbes = 0
    /\ motionCycles = 0
    /\ keyboardProbes' = 1
    /\ keyboardIngress' = TRUE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  streamState, motionCycles,
                  schedulerPolicy, rtBudgetBounded, admissionPending,
                  restoreFailed, hardLimitReached, pointerPositions, clock,
                  emitPermit, pointerIngress>>

EmitPointerPosition ==
    /\ streamState = Streaming
    /\ schedulerPolicy = BoundedRoundRobin
    /\ rtBudgetBounded
    /\ ~admissionPending
    /\ keyboardProbes = 1
    /\ emitPermit
    /\ motionCycles < MotionCycles
    /\ motionCycles' = motionCycles + 1
    /\ pointerPositions' = pointerPositions + 1
    /\ emitPermit' = FALSE
    /\ pointerIngress' = TRUE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  streamState, keyboardProbes, clock,
                  schedulerPolicy, rtBudgetBounded, admissionPending,
                  restoreFailed, hardLimitReached, keyboardIngress>>

CadenceTick ==
    /\ streamState = Streaming
    /\ schedulerPolicy = BoundedRoundRobin
    /\ rtBudgetBounded
    /\ ~admissionPending
    /\ ~emitPermit
    /\ clock < MaxClock
    /\ clock' = clock + 1
    /\ emitPermit' = TRUE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  streamState, keyboardProbes, motionCycles, pointerPositions,
                  schedulerPolicy, rtBudgetBounded, admissionPending,
                  restoreFailed, hardLimitReached,
                  keyboardIngress, pointerIngress>>

RequestStreamStop ==
    /\ streamState = Streaming
    /\ schedulerPolicy = BoundedRoundRobin
    /\ rtBudgetBounded
    /\ ~admissionPending
    /\ streamState' = Restoring
    /\ emitPermit' = FALSE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  schedulerPolicy, rtBudgetBounded, admissionPending,
                  restoreFailed, hardLimitReached, keyboardProbes,
                  motionCycles, pointerPositions, clock, keyboardIngress,
                  pointerIngress>>

RestoreInputScheduler ==
    /\ streamState = Restoring
    /\ schedulerPolicy = BoundedRoundRobin
    /\ rtBudgetBounded
    /\ streamState' = Closed
    /\ schedulerPolicy' = NormalScheduler
    /\ rtBudgetBounded' = FALSE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  admissionPending, restoreFailed, hardLimitReached,
                  keyboardProbes, motionCycles, pointerPositions, clock,
                  emitPermit, keyboardIngress, pointerIngress>>

FatalInputRestore ==
    /\ streamState = Restoring
    /\ schedulerPolicy = BoundedRoundRobin
    /\ rtBudgetBounded
    /\ streamState' = Terminated
    /\ schedulerPolicy' = NoScheduler
    /\ rtBudgetBounded' = FALSE
    /\ restoreFailed' = TRUE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  admissionPending, hardLimitReached, keyboardProbes,
                  motionCycles, pointerPositions, clock, emitPermit,
                  keyboardIngress, pointerIngress>>

HardLimitStop ==
    /\ streamState = Streaming
    /\ schedulerPolicy = BoundedRoundRobin
    /\ rtBudgetBounded
    /\ streamState' = Terminated
    /\ schedulerPolicy' = NoScheduler
    /\ rtBudgetBounded' = FALSE
    /\ admissionPending' = FALSE
    /\ hardLimitReached' = TRUE
    /\ emitPermit' = FALSE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  restoreFailed, keyboardProbes, motionCycles,
                  pointerPositions, clock, keyboardIngress, pointerIngress>>

Idle == UNCHANGED vars

AdmissionSettlement ==
    AdmitBoundedInputScheduler \/ RollbackInputAdmission \/
    FatalInputAdmissionRollback
RestoreSettlement == RestoreInputScheduler \/ FatalInputRestore

Next ==
    \/ ArmReceiverVector
    \/ ObservePolicyConsumer
    \/ OpenCompositeStream
    \/ InstallInputBudget
    \/ AdmitBoundedInputScheduler
    \/ RollbackInputAdmission
    \/ FatalInputAdmissionRollback
    \/ EmitKeyboardProbe
    \/ EmitPointerPosition
    \/ CadenceTick
    \/ RequestStreamStop
    \/ RestoreInputScheduler
    \/ FatalInputRestore
    \/ HardLimitStop
    \/ Idle

Spec == Init /\ [][Next]_vars /\ WF_vars(ArmReceiverVector) /\
        WF_vars(ObservePolicyConsumer) /\ WF_vars(OpenCompositeStream) /\
        WF_vars(AdmissionSettlement) /\ WF_vars(RestoreSettlement) /\
        WF_vars(EmitKeyboardProbe) /\ WF_vars(EmitPointerPosition) /\
        WF_vars(CadenceTick)

TypeOK ==
    /\ pointerCapability \in BOOLEAN
    /\ vectorReady \in BOOLEAN
    /\ policyConsumerReady \in BOOLEAN
    /\ streamState \in {Unopened, Streaming, Restoring, Closed, Terminated}
    /\ schedulerPolicy \in {NormalScheduler, BoundedRoundRobin, NoScheduler}
    /\ rtBudgetBounded \in BOOLEAN
    /\ admissionPending \in BOOLEAN
    /\ restoreFailed \in BOOLEAN
    /\ hardLimitReached \in BOOLEAN
    /\ keyboardProbes \in 0..1
    /\ motionCycles \in 0..MotionCycles
    /\ pointerPositions \in 0..MotionCycles
    /\ clock \in 0..MaxClock
    /\ emitPermit \in BOOLEAN
    /\ keyboardIngress \in BOOLEAN
    /\ pointerIngress \in BOOLEAN

PointerSelectionPrecedesStreaming ==
    streamState = Streaming => pointerCapability

ContinuousProductionRequiresTransportAndPolicyConsumer ==
    streamState = Streaming => vectorReady /\ policyConsumerReady

PolicyConsumerCannotPrecedeTransport == policyConsumerReady => vectorReady

KeyboardProbeIsBoundedAndNonRepeating ==
    keyboardProbes <= 1

PointerPositionsExactlyTrackSyntheticMotion ==
    pointerPositions = motionCycles

PointerMotionRequiresTheOneKeyboardProof ==
    motionCycles > 0 =>
        /\ keyboardProbes = 1
        /\ keyboardIngress

InputEmissionRequiresBoundedScheduler ==
    streamState = Streaming /\ (keyboardProbes > 0 \/ motionCycles > 0) =>
        /\ schedulerPolicy = BoundedRoundRobin
        /\ rtBudgetBounded
        /\ ~admissionPending

PartialAdmissionCannotEmit ==
    admissionPending =>
        /\ streamState = Streaming
        /\ schedulerPolicy = NormalScheduler
        /\ rtBudgetBounded
        /\ keyboardProbes = 0
        /\ motionCycles = 0

RestoringStreamCannotEmit ==
    streamState = Restoring =>
        /\ schedulerPolicy = BoundedRoundRobin
        /\ rtBudgetBounded
        /\ ~emitPermit

TerminalSchedulerStateHasNoAuthority ==
    streamState = Terminated =>
        /\ schedulerPolicy = NoScheduler
        /\ ~rtBudgetBounded
        /\ ~admissionPending
        /\ ~emitPermit

RestoreFailureIsTerminal == restoreFailed => streamState = Terminated

HardLimitIsTerminal == hardLimitReached => streamState = Terminated

EveryStreamStopSettles ==
    [](streamState = Restoring => <>
        ((streamState = Closed /\ schedulerPolicy = NormalScheduler /\
          ~rtBudgetBounded) \/
         (streamState = Terminated /\ restoreFailed)))

CompletedMotionEventuallyProvesBothRoutes ==
    motionCycles = MotionCycles => <>(keyboardIngress /\ pointerIngress)

=============================================================================
