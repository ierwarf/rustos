--------------------------- MODULE RuntimeControlRpc ---------------------------
EXTENDS Naturals

(*******************************************************************************
Models the response-admission rule in libs/runtime-control/src/lib.rs.

A successful runtimed reply must be for the exact RPC that the client sent.
Error replies intentionally retain compatibility with the server's opcode-zero
error envelope, but malformed status values, unknown opcodes, oversized
snapshots, and successful cross-RPC replies fail closed. This is paired with
Kani harnesses over response_payload_len so the model and executable boundary
share the same outcome matrix.
*******************************************************************************)

CONSTANT MaxPrograms

RequestOps == {"snapshot", "launch", "terminate", "ready"}
ResponseOps == RequestOps \cup {"unknown"}
StatusKinds == {"ok", "server-error", "positive", "minimum"}
Versions == {"current", "wrong"}
Outcomes == {"pending", "success", "server-error", "protocol", "overflow"}

VARIABLES requestOp, responseOp, status, version, count, outcome, payloadCount, responseReceived

vars == <<requestOp, responseOp, status, version, count, outcome, payloadCount, responseReceived>>

Classify(req, response, responseStatus, responseVersion, responseCount) ==
    IF responseVersion # "current" THEN "protocol"
    ELSE IF responseStatus = "server-error" THEN "server-error"
    ELSE IF responseStatus # "ok" THEN "protocol"
    ELSE IF response # req THEN "protocol"
    ELSE IF req = "snapshot" THEN
        IF responseCount <= MaxPrograms THEN "success" ELSE "overflow"
    ELSE IF responseCount = 0 THEN "success" ELSE "protocol"

Init ==
    /\ requestOp \in RequestOps
    /\ responseOp = "unknown"
    /\ status = "positive"
    /\ version = "wrong"
    /\ count = 0
    /\ outcome = "pending"
    /\ payloadCount = 0
    /\ responseReceived = FALSE

ReceiveResponse ==
    /\ ~responseReceived
    /\ responseOp' \in ResponseOps
    /\ status' \in StatusKinds
    /\ version' \in Versions
    /\ count' \in 0..(MaxPrograms + 1)
    /\ outcome' = Classify(requestOp, responseOp', status', version', count')
    /\ payloadCount' =
        IF outcome' = "success" /\ requestOp = "snapshot" THEN count' ELSE 0
    /\ responseReceived' = TRUE
    /\ UNCHANGED requestOp

Next == ReceiveResponse

TypeOK ==
    /\ requestOp \in RequestOps
    /\ responseOp \in ResponseOps
    /\ status \in StatusKinds
    /\ version \in Versions
    /\ count \in 0..(MaxPrograms + 1)
    /\ outcome \in Outcomes
    /\ payloadCount \in 0..MaxPrograms
    /\ responseReceived \in BOOLEAN

SuccessEchoesExactRequest ==
    outcome = "success" =>
        /\ version = "current"
        /\ status = "ok"
        /\ responseOp = requestOp

SuccessfulSnapshotIsBounded ==
    outcome = "success" /\ requestOp = "snapshot" =>
        /\ count <= MaxPrograms
        /\ payloadCount = count

SuccessfulCommandHasNoPayload ==
    outcome = "success" /\ requestOp # "snapshot" =>
        /\ count = 0
        /\ payloadCount = 0

MalformedStatusNeverSucceeds ==
    status \in {"positive", "minimum"} => outcome # "success"

ServerErrorRetainsFailure ==
    version = "current" /\ status = "server-error" => outcome = "server-error"

CrossRpcSuccessIsImpossible ==
    responseOp # requestOp => outcome # "success"

ResolvedResponseIsTerminal == responseReceived => outcome # "pending"

Spec == Init /\ [][Next]_vars
================================================================================
