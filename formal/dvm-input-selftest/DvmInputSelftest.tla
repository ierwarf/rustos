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

CONSTANT MotionCycles

Unopened == "unopened"
Streaming == "streaming"

VARIABLES pointerCapability,
          vectorReady,
          policyConsumerReady,
          streamState,
          keyboardProbes,
          motionCycles,
          pointerPositions,
          keyboardIngress,
          pointerIngress

vars == <<pointerCapability, vectorReady, policyConsumerReady,
          streamState, keyboardProbes, motionCycles,
          pointerPositions, keyboardIngress, pointerIngress>>

Init ==
    /\ pointerCapability = TRUE
    /\ vectorReady = FALSE
    /\ policyConsumerReady = FALSE
    /\ streamState = Unopened
    /\ keyboardProbes = 0
    /\ motionCycles = 0
    /\ pointerPositions = 0
    /\ keyboardIngress = FALSE
    /\ pointerIngress = FALSE

ArmReceiverVector ==
    /\ ~vectorReady
    /\ vectorReady' = TRUE
    /\ UNCHANGED <<pointerCapability, policyConsumerReady, streamState,
                  keyboardProbes, motionCycles, pointerPositions,
                  keyboardIngress, pointerIngress>>

ObservePolicyConsumer ==
    /\ vectorReady
    /\ ~policyConsumerReady
    /\ policyConsumerReady' = TRUE
    /\ UNCHANGED <<pointerCapability, vectorReady, streamState,
                  keyboardProbes, motionCycles, pointerPositions,
                  keyboardIngress, pointerIngress>>

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
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  keyboardProbes, motionCycles,
                  pointerPositions, keyboardIngress, pointerIngress>>

(*******************************************************************************
One F12-only probe establishes keyboard ingress.  It is bounded to one
press/release pair; subsequent self-test work is pointer-only and therefore
cannot turn duration into console command pressure.
*******************************************************************************)
EmitKeyboardProbe ==
    /\ streamState = Streaming
    /\ keyboardProbes = 0
    /\ motionCycles = 0
    /\ keyboardProbes' = 1
    /\ keyboardIngress' = TRUE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  streamState, motionCycles,
                  pointerPositions, pointerIngress>>

EmitPointerPosition ==
    /\ streamState = Streaming
    /\ keyboardProbes = 1
    /\ motionCycles < MotionCycles
    /\ motionCycles' = motionCycles + 1
    /\ pointerPositions' = pointerPositions + 1
    /\ pointerIngress' = TRUE
    /\ UNCHANGED <<pointerCapability, vectorReady, policyConsumerReady,
                  streamState, keyboardProbes,
                  keyboardIngress>>

Idle == UNCHANGED vars

Next ==
    \/ ArmReceiverVector
    \/ ObservePolicyConsumer
    \/ OpenCompositeStream
    \/ EmitKeyboardProbe
    \/ EmitPointerPosition
    \/ Idle

Spec == Init /\ [][Next]_vars /\ WF_vars(ArmReceiverVector) /\
        WF_vars(ObservePolicyConsumer) /\ WF_vars(OpenCompositeStream) /\
        WF_vars(EmitKeyboardProbe) /\ WF_vars(EmitPointerPosition)

TypeOK ==
    /\ pointerCapability \in BOOLEAN
    /\ vectorReady \in BOOLEAN
    /\ policyConsumerReady \in BOOLEAN
    /\ streamState \in {Unopened, Streaming}
    /\ keyboardProbes \in 0..1
    /\ motionCycles \in 0..MotionCycles
    /\ pointerPositions \in 0..MotionCycles
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

CompletedMotionEventuallyProvesBothRoutes ==
    motionCycles = MotionCycles => <>(keyboardIngress /\ pointerIngress)

=============================================================================
