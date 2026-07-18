------------------------- MODULE WaylandFramePacing -------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models the uiserver Wayland frame-callback and presentation boundary.

Concrete owners:
  * services/uiserver/src/main.rs main present loop
  * services/uiserver/src/wayland.rs `consume_frame_callback_permit`

Input and Wayland damage are accumulated before one main presentation.  There
is no cursor-only or Wayland-only early-present action.  A successful real
presentation grants one non-accumulating frame-callback permit.  The boot
presentation supplies the initial permit.  A callback-only commit may instead
use one non-accumulating cadence permit, but only while no visual damage is
pending.  Either callback route consumes its permit.

The model proves ownership, coalescing, and eventual progress under the named
presentation/cadence fairness assumptions.  It does not prove a frame-rate or
transport-latency bound; those remain measured KVM acceptance gates.
*******************************************************************************)

NoPresentation == {}
InputPresentation == {"input"}
WaylandPresentation == {"wayland"}
CombinedPresentation == {"input", "wayland"}

NoCallback == "none"
FrameCallback == "frame-present"
CadenceCallback == "callback-only-cadence"

VARIABLES inputDamage,
          waylandDamage,
          callbackPending,
          framePermit,
          cadencePermit,
          lastPresentation,
          lastPendingAtPresent,
          lastCallbackSource,
          lastCallbackHadPermit,
          presentationEpoch,
          callbackEpoch

vars == <<inputDamage, waylandDamage, callbackPending, framePermit,
          cadencePermit, lastPresentation, lastPendingAtPresent,
          lastCallbackSource, lastCallbackHadPermit, presentationEpoch,
          callbackEpoch>>

Init ==
    /\ inputDamage = FALSE
    /\ waylandDamage = FALSE
    /\ callbackPending = FALSE
    /\ framePermit = TRUE
    /\ cadencePermit = FALSE
    /\ lastPresentation = NoPresentation
    /\ lastPendingAtPresent = NoPresentation
    /\ lastCallbackSource = NoCallback
    /\ lastCallbackHadPermit = FALSE
    /\ presentationEpoch = 0
    /\ callbackEpoch = 0

InputArrives ==
    /\ ~inputDamage
    /\ inputDamage' = TRUE
    /\ UNCHANGED <<waylandDamage, callbackPending, framePermit,
                  cadencePermit, lastPresentation, lastCallbackSource,
                  lastPendingAtPresent, lastCallbackHadPermit,
                  presentationEpoch, callbackEpoch>>

WaylandDamageCommit ==
    /\ ~waylandDamage
    /\ waylandDamage' = TRUE
    /\ callbackPending' = TRUE
    /\ UNCHANGED <<inputDamage, framePermit, cadencePermit,
                  lastPresentation, lastPendingAtPresent, lastCallbackSource,
                  lastCallbackHadPermit, presentationEpoch, callbackEpoch>>

WaylandCallbackOnlyCommit ==
    /\ ~callbackPending
    /\ ~waylandDamage
    /\ callbackPending' = TRUE
    /\ UNCHANGED <<inputDamage, waylandDamage, framePermit, cadencePermit,
                  lastPresentation, lastPendingAtPresent, lastCallbackSource,
                  lastCallbackHadPermit, presentationEpoch, callbackEpoch>>

(*******************************************************************************
The real timer deadline is monotonic and does not accumulate missed pulses.
The Boolean abstraction preserves precisely that one-permit capacity.
*******************************************************************************)
CadenceDeadlineArrives ==
    /\ ~cadencePermit
    /\ cadencePermit' = TRUE
    /\ UNCHANGED <<inputDamage, waylandDamage, callbackPending, framePermit,
                  lastPresentation, lastPendingAtPresent, lastCallbackSource,
                  lastCallbackHadPermit, presentationEpoch, callbackEpoch>>

(*******************************************************************************
One presentation atomically snapshots and consumes every class of visual
damage that was pending at its linearization point.  Replenishing an unused
frame permit overwrites TRUE with TRUE rather than accumulating credit.
*******************************************************************************)
PresentCombined ==
    /\ inputDamage \/ waylandDamage
    /\ lastPresentation' =
          {kind \in {"input", "wayland"} :
              (kind = "input" /\ inputDamage) \/
              (kind = "wayland" /\ waylandDamage)}
    /\ lastPendingAtPresent' =
          {kind \in {"input", "wayland"} :
              (kind = "input" /\ inputDamage) \/
              (kind = "wayland" /\ waylandDamage)}
    /\ inputDamage' = FALSE
    /\ waylandDamage' = FALSE
    /\ framePermit' = TRUE
    /\ presentationEpoch' = (presentationEpoch + 1) % 3
    /\ UNCHANGED <<callbackPending, cadencePermit, lastCallbackSource,
                  lastCallbackHadPermit, callbackEpoch>>

SendFromFramePermit ==
    /\ callbackPending
    /\ framePermit
    /\ callbackPending' = FALSE
    /\ framePermit' = FALSE
    /\ cadencePermit' = FALSE
    /\ lastCallbackSource' = FrameCallback
    /\ lastCallbackHadPermit' = framePermit
    /\ callbackEpoch' = (callbackEpoch + 1) % 3
    /\ UNCHANGED <<inputDamage, waylandDamage, lastPresentation,
                  lastPendingAtPresent, presentationEpoch>>

SendCallbackOnlyFromCadence ==
    /\ callbackPending
    /\ ~inputDamage
    /\ ~waylandDamage
    /\ ~framePermit
    /\ cadencePermit
    /\ callbackPending' = FALSE
    /\ cadencePermit' = FALSE
    /\ lastCallbackSource' = CadenceCallback
    /\ lastCallbackHadPermit' =
          cadencePermit /\ ~framePermit /\ ~inputDamage /\ ~waylandDamage
    /\ callbackEpoch' = (callbackEpoch + 1) % 3
    /\ UNCHANGED <<inputDamage, waylandDamage, framePermit,
                  lastPresentation, lastPendingAtPresent, presentationEpoch>>

Next ==
    \/ InputArrives
    \/ WaylandDamageCommit
    \/ WaylandCallbackOnlyCommit
    \/ CadenceDeadlineArrives
    \/ PresentCombined
    \/ SendFromFramePermit
    \/ SendCallbackOnlyFromCadence

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(PresentCombined)
    /\ WF_vars(CadenceDeadlineArrives)
    /\ WF_vars(SendFromFramePermit)
    /\ WF_vars(SendCallbackOnlyFromCadence)

TypeOK ==
    /\ inputDamage \in BOOLEAN
    /\ waylandDamage \in BOOLEAN
    /\ callbackPending \in BOOLEAN
    /\ framePermit \in BOOLEAN
    /\ cadencePermit \in BOOLEAN
    /\ lastPresentation \in
          {NoPresentation, InputPresentation, WaylandPresentation,
           CombinedPresentation}
    /\ lastPendingAtPresent \in
          {NoPresentation, InputPresentation, WaylandPresentation,
           CombinedPresentation}
    /\ lastCallbackSource \in
          {NoCallback, FrameCallback, CadenceCallback}
    /\ lastCallbackHadPermit \in BOOLEAN
    /\ presentationEpoch \in 0..2
    /\ callbackEpoch \in 0..2

PresentationIsCoalesced ==
    /\ lastPresentation = lastPendingAtPresent
    /\ (presentationEpoch = 0) \/ lastPresentation # NoPresentation

CallbackSourceIsAuthorized ==
    lastCallbackSource = NoCallback \/ lastCallbackHadPermit

DamageEventuallyPresented ==
    (inputDamage \/ waylandDamage) ~> (~inputDamage /\ ~waylandDamage)

CallbackEventuallyReleased == callbackPending ~> ~callbackPending

=============================================================================
