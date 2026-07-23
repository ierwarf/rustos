---------------------- MODULE CommercialServiceEnvelope ----------------------
EXTENDS Naturals

(*******************************************************************************
Composes the shared commercial request validator, one service dispatch, and
the caller-side exact response validator.

A malformed request receives an explicit error reply.  A valid request may
complete, fail, lose its peer, or hit the finite IPC deadline, but a response
with a foreign header, reserved field, nested length, descriptor count, or
truncated wire image can never be admitted as authority.

Concrete owners:
  * libs/rustos-user-abi/src/syscall.rs
  * commercial service handlers
  * kernel and service commercial clients
*******************************************************************************)

RequestShapes == {"exact", "malformed"}
ReplyShapes == {"exact", "foreign", "truncated", "reserved", "oversized"}
TerminalPhases ==
    {"malformed-replied", "admitted", "reply-rejected", "peer-closed", "timed-out"}

VARIABLES phase, requestShape, replyShape

vars == <<phase, requestShape, replyShape>>

Init ==
    /\ phase = "received"
    /\ requestShape \in RequestShapes
    /\ replyShape \in ReplyShapes

RejectMalformed ==
    /\ phase = "received"
    /\ requestShape = "malformed"
    /\ phase' = "malformed-replied"
    /\ UNCHANGED <<requestShape, replyShape>>

Dispatch ==
    /\ phase = "received"
    /\ requestShape = "exact"
    /\ phase' = "waiting"
    /\ UNCHANGED <<requestShape, replyShape>>

AdmitExactReply ==
    /\ phase = "waiting"
    /\ replyShape = "exact"
    /\ phase' = "admitted"
    /\ UNCHANGED <<requestShape, replyShape>>

RejectBadReply ==
    /\ phase = "waiting"
    /\ replyShape # "exact"
    /\ phase' = "reply-rejected"
    /\ UNCHANGED <<requestShape, replyShape>>

PeerClosed ==
    /\ phase = "waiting"
    /\ phase' = "peer-closed"
    /\ UNCHANGED <<requestShape, replyShape>>

Timeout ==
    /\ phase = "waiting"
    /\ phase' = "timed-out"
    /\ UNCHANGED <<requestShape, replyShape>>

Next ==
    \/ RejectMalformed
    \/ Dispatch
    \/ AdmitExactReply
    \/ RejectBadReply
    \/ PeerClosed
    \/ Timeout

TypeOK ==
    /\ phase \in {"received", "waiting"} \cup TerminalPhases
    /\ requestShape \in RequestShapes
    /\ replyShape \in ReplyShapes

MalformedRequestNeverDispatches ==
    requestShape = "malformed" => phase # "waiting"

AdmissionRequiresExactRequestAndReply ==
    phase = "admitted" =>
        /\ requestShape = "exact"
        /\ replyShape = "exact"

MalformedRequestGetsExplicitReply ==
    requestShape = "malformed" /\ phase \in TerminalPhases =>
        phase = "malformed-replied"

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

EventuallyTerminal == <> (phase \in TerminalPhases)
=============================================================================
