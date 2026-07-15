------------------------------ MODULE UiInputMotion -----------------------------
EXTENDS Integers, Naturals

(*******************************************************************************
Models the DVM-only `--exercise-input` pointer trajectory used by the KVM FPS
gate.

Concrete owner:
  driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c
  `input_selftest_emit_cycle`

The synthetic absolute device must keep producing visible cursor movement even when an
earlier run left the cursor at an arbitrary screen edge.  A one-direction
position stream can be accepted and counted as input while becoming visually
stationary after clamping; that is not evidence of render throughput.  The
four-phase trajectory is an axis-aligned square: exactly one axis moves on
each leg. The concrete source is polled at most every 8 ms, while the product
gate requires at least 55 accepted and 50 visibly presented updates per
one-second window. This model proves that no accepted visible motion epoch is
lost and that both axes receive visible work.

This model explores every initial cursor position.  It proves that the phase
transition is well-formed, cursor coordinates remain in range, a finite test
window contains a minimum amount of visible work, and its final visual state
is eventually presented under the uiserver's presentation fairness.  The
separate `ui-frame-budget` model owns the policy-IPC and worker-stall proof.
*******************************************************************************)

CONSTANTS MaxX, MaxY, PhaseCycles, EmissionCount, RequiredVisibleUpdates,
          RequiredAxisUpdates

VARIABLES emitted,
          phase,
          phaseTick,
          cursorX,
          cursorY,
          consecutiveClamped,
          motionEpoch,
          xMotionUpdates,
          yMotionUpdates,
          presentedEpoch,
          frameDebt

vars == <<emitted, phase, phaseTick, cursorX, cursorY, consecutiveClamped,
          motionEpoch, xMotionUpdates, yMotionUpdates, presentedEpoch,
          frameDebt>>

Clamp(value, maximum) ==
    IF value < 0 THEN 0 ELSE IF value > maximum THEN maximum ELSE value

Dx == CASE phase = 0 -> 1
        [] phase = 2 -> -1
        [] OTHER -> 0
Dy == CASE phase = 1 -> 1
        [] phase = 3 -> -1
        [] OTHER -> 0

NextPhaseTick == (phaseTick + 1) % PhaseCycles
NextPhase == IF NextPhaseTick = 0 THEN (phase + 1) % 4 ELSE phase

Init ==
    /\ emitted = 0
    /\ phase = 0
    /\ phaseTick = 0
    /\ cursorX \in 0..MaxX
    /\ cursorY \in 0..MaxY
    /\ consecutiveClamped = 0
    /\ motionEpoch = 0
    /\ xMotionUpdates = 0
    /\ yMotionUpdates = 0
    /\ presentedEpoch = 0
    /\ frameDebt = FALSE

(*******************************************************************************
Each emission matches one self-test cycle.  A phase lasts a fixed, short
number of cycles; reversing either axis before it can remain permanently
clamped guarantees a later visual update without requiring knowledge of the
desktop's current cursor location.
*******************************************************************************)
EmitPointer ==
    /\ emitted < EmissionCount
    /\ LET nextX == Clamp(cursorX + Dx, MaxX) IN
       LET nextY == Clamp(cursorY + Dy, MaxY) IN
       LET moved == nextX # cursorX \/ nextY # cursorY IN
       /\ emitted' = emitted + 1
       /\ phase' = NextPhase
       /\ phaseTick' = NextPhaseTick
       /\ cursorX' = nextX
       /\ cursorY' = nextY
       /\ consecutiveClamped' = IF moved THEN 0 ELSE consecutiveClamped + 1
       /\ motionEpoch' = IF moved THEN motionEpoch + 1 ELSE motionEpoch
       /\ xMotionUpdates' = IF nextX # cursorX THEN xMotionUpdates + 1
                             ELSE xMotionUpdates
       /\ yMotionUpdates' = IF nextY # cursorY THEN yMotionUpdates + 1
                             ELSE yMotionUpdates
       /\ presentedEpoch' = presentedEpoch
       /\ frameDebt' = IF moved THEN TRUE ELSE frameDebt

(*******************************************************************************
Present commits the newest visible input state.  It intentionally coalesces
multiple motion updates: rendering need not equal input-event count, but no
visible state may remain permanently unpresented after the finite test ends.
*******************************************************************************)
Present ==
    /\ frameDebt
    /\ presentedEpoch' = motionEpoch
    /\ frameDebt' = FALSE
    /\ UNCHANGED <<emitted, phase, phaseTick, cursorX, cursorY,
                  consecutiveClamped, motionEpoch, xMotionUpdates,
                  yMotionUpdates>>

Idle ==
    /\ emitted = EmissionCount
    /\ ~frameDebt
    /\ UNCHANGED vars

Next == EmitPointer \/ Present \/ Idle

Spec == Init /\ [][Next]_vars /\ WF_vars(EmitPointer) /\ WF_vars(Present)

TypeOK ==
    /\ emitted \in 0..EmissionCount
    /\ phase \in 0..3
    /\ phaseTick \in 0..(PhaseCycles - 1)
    /\ cursorX \in 0..MaxX
    /\ cursorY \in 0..MaxY
    /\ consecutiveClamped \in Nat
    /\ motionEpoch \in Nat
    /\ xMotionUpdates \in Nat
    /\ yMotionUpdates \in Nat
    /\ presentedEpoch \in Nat
    /\ frameDebt \in BOOLEAN

PhaseAndCursorSafety ==
    /\ consecutiveClamped <= 2 * PhaseCycles
    /\ Dx \in {-1, 0, 1}
    /\ Dy \in {-1, 0, 1}
    /\ (Dx = 0) # (Dy = 0)
    /\ presentedEpoch <= motionEpoch
    /\ frameDebt = (motionEpoch > presentedEpoch)

(*******************************************************************************
For every explored initial cursor position, a bounded performance sample has
enough actual cursor changes to make the KVM FPS threshold meaningful.
*******************************************************************************)
SustainedInputHasVisibleWork ==
    emitted = EmissionCount =>
        /\ motionEpoch >= RequiredVisibleUpdates
        /\ xMotionUpdates >= RequiredAxisUpdates
        /\ yMotionUpdates >= RequiredAxisUpdates

FinalVisualStateEventuallyPresented ==
    (emitted = EmissionCount) ~> (presentedEpoch = motionEpoch)

EveryVisibleMotionEventuallyPresented ==
    \A epoch \in 1..EmissionCount :
        (motionEpoch >= epoch) ~> (presentedEpoch >= epoch)

=============================================================================
