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
NormalScheduler == "normal"
BoundedRoundRobin == "bounded-rr"

VARIABLES pointerCapability,
          vectorReady,
          policyConsumerReady,
          streamState,
          schedulerPolicy,
          rtBudgetBounded,
          keyboardProbes,
          motionCycles,
          pointerPositions,
          clock,
          emitPermit,
          keyboardIngress,
          pointerIngress

vars == <<pointerCapability, vectorReady, policyConsumerReady,
          streamState, schedulerPolicy, rtBudgetBounded,
          keyboardProbes, motionCycles,
          pointerPositions, clock, emitPermit, keyboardIngress, pointerIngress>>

Init ==
    /\ pointerCapability = TRUE
    /\ vectorReady = FALSE
    /\ policyConsumerReady = FALSE
    /\ streamState = Unopened
    /\ schedulerPolicy = NormalScheduler
    /\ rtBudgetBounded = FALSE
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
                  keyboardProbes, motionCycles, pointerPositions,
                  clock, emitPermit, keyboardIngress, pointerIngress>>

ObservePolicyConsumer ==
    /\ vectorReady
    /\ ~policyConsumerReady
    /\ policyConsumerReady' = TRUE
    /\ UNCHANGED <<pointerCapability, vectorReady, streamState,
                  schedulerPolicy, rtBudgetBounded,
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
                  keyboardProbes, motionCycles,
                  pointerPositions, clock, keyboardIngress, pointerIngress>>

(*******************************************************************************
The authenticated input stream receives a low SCHED_RR priority only after a
strict RLIMIT_RTTIME ceiling is installed and read back. This bounds guest
scheduling latency while a runaway relay is terminated instead of starving
KMS or recovery indefinitely.
*******************************************************************************)
AdmitBoundedInputScheduler ==
    /\ streamState = Streaming
    /\ schedulerPolicy = NormalScheduler
    /\ ~rtBudgetBounded
    /\ schedulerPolicy' = BoundedRoundRobin
    /\ rtBudgetBounded' = TRUE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  streamState, keyboardProbes, motionCycles, pointerPositions,
                  clock, emitPermit, keyboardIngress, pointerIngress>>

(*******************************************************************************
One F12-only probe establishes keyboard ingress.  It is bounded to one
press/release pair; subsequent self-test work is pointer-only and therefore
cannot turn duration into console command pressure.
*******************************************************************************)
EmitKeyboardProbe ==
    /\ streamState = Streaming
    /\ schedulerPolicy = BoundedRoundRobin
    /\ rtBudgetBounded
    /\ keyboardProbes = 0
    /\ motionCycles = 0
    /\ keyboardProbes' = 1
    /\ keyboardIngress' = TRUE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  streamState, motionCycles,
                  schedulerPolicy, rtBudgetBounded, pointerPositions, clock,
                  emitPermit, pointerIngress>>

EmitPointerPosition ==
    /\ streamState = Streaming
    /\ schedulerPolicy = BoundedRoundRobin
    /\ rtBudgetBounded
    /\ keyboardProbes = 1
    /\ emitPermit
    /\ motionCycles < MotionCycles
    /\ motionCycles' = motionCycles + 1
    /\ pointerPositions' = pointerPositions + 1
    /\ emitPermit' = FALSE
    /\ pointerIngress' = TRUE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  streamState, keyboardProbes, clock,
                  schedulerPolicy, rtBudgetBounded, keyboardIngress>>

CadenceTick ==
    /\ streamState = Streaming
    /\ ~emitPermit
    /\ clock < MaxClock
    /\ clock' = clock + 1
    /\ emitPermit' = TRUE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  streamState, keyboardProbes, motionCycles, pointerPositions,
                  schedulerPolicy, rtBudgetBounded,
                  keyboardIngress, pointerIngress>>

Idle == UNCHANGED vars

Next ==
    \/ ArmReceiverVector
    \/ ObservePolicyConsumer
    \/ OpenCompositeStream
    \/ AdmitBoundedInputScheduler
    \/ EmitKeyboardProbe
    \/ EmitPointerPosition
    \/ CadenceTick
    \/ Idle

Spec == Init /\ [][Next]_vars /\ WF_vars(ArmReceiverVector) /\
        WF_vars(ObservePolicyConsumer) /\ WF_vars(OpenCompositeStream) /\
        WF_vars(AdmitBoundedInputScheduler) /\
        WF_vars(EmitKeyboardProbe) /\ WF_vars(EmitPointerPosition) /\
        WF_vars(CadenceTick)

TypeOK ==
    /\ pointerCapability \in BOOLEAN
    /\ vectorReady \in BOOLEAN
    /\ policyConsumerReady \in BOOLEAN
    /\ streamState \in {Unopened, Streaming}
    /\ schedulerPolicy \in {NormalScheduler, BoundedRoundRobin}
    /\ rtBudgetBounded \in BOOLEAN
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
    keyboardProbes > 0 \/ motionCycles > 0 =>
        /\ schedulerPolicy = BoundedRoundRobin
        /\ rtBudgetBounded

CompletedMotionEventuallyProvesBothRoutes ==
    motionCycles = MotionCycles => <>(keyboardIngress /\ pointerIngress)

=============================================================================
