-------------------- MODULE ExceptionDiagnosticCustody --------------------
EXTENDS Naturals

(***************************************************************************
An ordinary user exception's breadcrumb shares the existing bounded serialized
sink with structured frames. A supervisor/fatal breadcrumb must retain raw,
lock-free emergency output. The aligned dispatcher's sink choice is projected
here; panic/NMI direct emergency sites and actual port I/O are not executed.
frame=1 represents another CPU holding the structured-output owner mid-frame.
***************************************************************************)
VARIABLES user, route, frame, markerDone, corrupted
vars == <<user, route, frame, markerDone, corrupted>>
Init ==
    /\ user \in BOOLEAN /\ route = "none" /\ frame = 0
    /\ markerDone = FALSE /\ corrupted = FALSE

Route ==
    /\ route = "none"
    /\ route' = IF user THEN "serialized" ELSE "raw" \* ordinary user is not emergency
    /\ UNCHANGED <<user, frame, markerDone, corrupted>>

FramePrefix ==
    /\ frame = 0 /\ frame' = 1
    /\ UNCHANGED <<user, route, markerDone, corrupted>>

FrameSuffix ==
    /\ frame = 1 /\ frame' = 2
    /\ UNCHANGED <<user, route, markerDone, corrupted>>

SerializedMarker ==
    /\ route = "serialized" /\ ~markerDone
    /\ frame # 1 \* normal output cannot splice a held structured frame
    /\ markerDone' = TRUE
    /\ corrupted' = (corrupted \/ (frame = 1))
    /\ UNCHANGED <<user, route, frame>>

RawMarker ==
    /\ route = "raw" /\ ~markerDone
    /\ markerDone' = TRUE
    /\ corrupted' = (corrupted \/ (frame = 1))
    /\ UNCHANGED <<user, route, frame>>

Next == Route \/ FramePrefix \/ FrameSuffix \/ SerializedMarker \/ RawMarker \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars
TypeOK ==
    /\ user \in BOOLEAN /\ route \in {"none", "serialized", "raw"}
    /\ frame \in 0..2 /\ markerDone \in BOOLEAN /\ corrupted \in BOOLEAN
UserChoosesSerialized == user /\ route # "none" => route = "serialized"
FatalChoosesEmergency == ~user /\ route # "none" => route = "raw"
UserCannotCorruptStructuredFrame == user => ~corrupted
=============================================================================
