------------------------------ MODULE UiInputMotion -----------------------------
EXTENDS Integers, Naturals

(*******************************************************************************
Models the DVM-only `--exercise-input` pointer trajectory used by the KVM FPS
gate.

Concrete owner:
  driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c
  `input_selftest_emit_cycle`

The synthetic device must keep producing visible cursor movement even when an
earlier run left the cursor at an arbitrary screen edge.  A one-direction
relative stream can be accepted and counted as input while becoming visually
stationary after clamping; that is not evidence of render throughput.  The
four-phase square trajectory changes direction independently on each axis.

This model explores every initial cursor position.  It proves that the phase
transition is well-formed, cursor coordinates remain in range, a finite test
window contains a minimum amount of visible work, and its final visual state
is eventually presented under the uiserver's presentation fairness.  The
separate `ui-frame-budget` model owns the policy-IPC and worker-stall proof.
*******************************************************************************)

CONSTANTS MaxX, MaxY, PhaseCycles, EmissionCount, RequiredVisibleUpdates

VARIABLES emitted,
          phase,
          phaseTick,
          cursorX,
          cursorY,
          consecutiveClamped,
          motionEpoch,
          presentedEpoch,
          frameDebt

vars == <<emitted, phase, phaseTick, cursorX, cursorY, consecutiveClamped,
          motionEpoch, presentedEpoch, frameDebt>>

Clamp(value, maximum) ==
    IF value < 0 THEN 0 ELSE IF value > maximum THEN maximum ELSE value

Dx == IF phase \in {0, 2} THEN 1 ELSE -1
Dy == IF phase \in {0, 1} THEN 1 ELSE -1

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
                  consecutiveClamped, motionEpoch>>

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
    /\ presentedEpoch \in Nat
    /\ frameDebt \in BOOLEAN

PhaseAndCursorSafety ==
    /\ consecutiveClamped <= PhaseCycles
    /\ presentedEpoch <= motionEpoch
    /\ frameDebt = (motionEpoch > presentedEpoch)

(*******************************************************************************
For every explored initial cursor position, a bounded performance sample has
enough actual cursor changes to make the KVM FPS threshold meaningful.
*******************************************************************************)
SustainedInputHasVisibleWork ==
    emitted = EmissionCount => motionEpoch >= RequiredVisibleUpdates

FinalVisualStateEventuallyPresented ==
    (emitted = EmissionCount) ~> (presentedEpoch = motionEpoch)

=============================================================================
