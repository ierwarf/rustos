------------------------ MODULE NetworkPayloadSession ------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Owner: netd for IPv4 policy; kernel io-manager for bounded authenticated DVM
Ethernet transport. Linearization point: AdmitPayload or DropPayload advances
the one consumer cursor exactly once, so malformed input cannot poison a slot.
*******************************************************************************)

CONSTANTS Frames, MaxQueue, MaxEpoch, MaxCursor

ValidFrame(frame) == frame \in {"ipv4", "arp"}

VARIABLES epoch, active, queue, delivered, dropped, consumer
vars == <<epoch, active, queue, delivered, dropped, consumer>>

Init ==
    /\ epoch = 0
    /\ active = FALSE
    /\ queue = {}
    /\ delivered = {}
    /\ dropped = {}
    /\ consumer = 0

Activate ==
    /\ ~active
    /\ epoch' = (epoch % MaxEpoch) + 1
    /\ active' = TRUE
    /\ queue' = {}
    /\ UNCHANGED <<delivered, dropped, consumer>>

Enqueue(frame) ==
    /\ active
    /\ frame \notin queue
    /\ Cardinality(queue) < MaxQueue
    /\ queue' = queue \cup {frame}
    /\ UNCHANGED <<epoch, active, delivered, dropped, consumer>>

AdmitPayload(frame) ==
    /\ active
    /\ frame \in queue
    /\ ValidFrame(frame)
    /\ queue' = queue \ {frame}
    /\ delivered' = delivered \cup {<<epoch, frame>>}
    /\ consumer' = (consumer + 1) % (MaxCursor + 1)
    /\ UNCHANGED <<epoch, active, dropped>>

DropPayload(frame) ==
    /\ active
    /\ frame \in queue
    /\ ~ValidFrame(frame)
    /\ queue' = queue \ {frame}
    /\ dropped' = dropped \cup {<<epoch, frame>>}
    /\ consumer' = (consumer + 1) % (MaxCursor + 1)
    /\ UNCHANGED <<epoch, active, delivered>>

Revoke ==
    /\ active
    /\ active' = FALSE
    /\ dropped' = dropped \cup ({epoch} \X queue)
    /\ consumer' = (consumer + Cardinality(queue)) % (MaxCursor + 1)
    /\ queue' = {}
    /\ UNCHANGED <<epoch, delivered>>

Next ==
    \/ Activate
    \/ \E frame \in Frames: Enqueue(frame)
    \/ \E frame \in Frames: AdmitPayload(frame)
    \/ \E frame \in Frames: DropPayload(frame)
    \/ Revoke

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ epoch \in 0..MaxEpoch
    /\ active \in BOOLEAN
    /\ queue \in SUBSET Frames
    /\ delivered \in SUBSET ((0..MaxEpoch) \X Frames)
    /\ dropped \in SUBSET ((0..MaxEpoch) \X Frames)
    /\ consumer \in 0..MaxCursor

QueueIsBounded == Cardinality(queue) <= MaxQueue
OnlyValidPayloadDelivered == \A item \in delivered: ValidFrame(item[2])
InactiveSessionHasNoQueuedPayload == ~active => queue = {}
DeliveredEpochIsAuthenticated == \A item \in delivered: item[1] \in 1..MaxEpoch

=============================================================================
