----------------------------- MODULE DvmVolumeIo -----------------------------
EXTENDS Naturals

(*******************************************************************************
Models the service-to-kernel storage-DVM I/O contract after a generation has
been admitted. Provider geometry must bind the exact response generation and
capacity, use a supported bounded sector size and flags, and have a nonzero
non-overflowing byte length. A malformed FAT image cannot become a mounted
volume. The request must then be non-empty, block aligned, and wholly contained
in the published volume before DVM dispatch. Chunk arithmetic may not wrap,
and recoverable broker failures retain their distinct outcome rather than
collapsing into a fabricated generic device fault. The composed
vfsd-to-storaged bulk response is admitted only when its complete request
header, generation, LBA, block count, reserved fields, and payload length bind
the requested chunk; a foreign, stale, or oversized reply never advances the
filesystem cursor.

Configured read, mutation, and flush failures are admitted only after request
validation and before any request/ring authority is published. A real
post-publication transport fault remains a distinct modeled path.

Concrete owners:
  * services/vfsd/src/block.rs
  * services/storaged/src/block.rs
  * kernel/compat/src/user/syscall/linux/block_broker_ops.rs
  * kernel/io-manager/src/io/dvm_block.rs
*******************************************************************************)

CONSTANT MaxChunks

RequestKinds == {"valid", "empty", "overflow", "overrun"}
GeometryKinds == {"valid", "zero", "bad-sector", "overflow", "foreign", "unknown-flags"}
ImageKinds == {"valid", "malformed"}
BrokerOutcomes == {"ok", "timeout", "not-present", "device-fault"}
ReplyBindings == {"exact", "wrong-header", "wrong-generation", "wrong-range", "oversized"}
FaultKinds == {"none", "read", "mutation", "flush"}
TerminalPhases ==
    {"geometry-rejected", "image-rejected", "rejected", "reply-rejected",
     "complete", "timed-out", "unavailable", "failed"}

VARIABLES phase, geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          faultKind, totalChunks, completedChunks, requestPublished

vars ==
    <<phase, geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
      faultKind, totalChunks, completedChunks, requestPublished>>

Init ==
    /\ phase = "idle"
    /\ geometryKind \in GeometryKinds
    /\ imageKind \in ImageKinds
    /\ requestKind \in RequestKinds
    /\ brokerOutcome \in BrokerOutcomes
    /\ replyBinding \in ReplyBindings
    /\ faultKind \in FaultKinds
    /\ faultKind # "none" => brokerOutcome = "ok"
    /\ totalChunks \in 1..MaxChunks
    /\ completedChunks = 0
    /\ requestPublished = FALSE

AdmitGeometry ==
    /\ phase = "idle"
    /\ geometryKind = "valid"
    /\ phase' = "geometry-admitted"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          faultKind, totalChunks, completedChunks, requestPublished>>

RejectGeometry ==
    /\ phase = "idle"
    /\ geometryKind # "valid"
    /\ phase' = "geometry-rejected"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          faultKind, totalChunks, completedChunks, requestPublished>>

AdmitImage ==
    /\ phase = "geometry-admitted"
    /\ imageKind = "valid"
    /\ phase' = "volume-admitted"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          faultKind, totalChunks, completedChunks, requestPublished>>

RejectImage ==
    /\ phase = "geometry-admitted"
    /\ imageKind # "valid"
    /\ phase' = "image-rejected"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          faultKind, totalChunks, completedChunks, requestPublished>>

Accept ==
    /\ phase = "volume-admitted"
    /\ requestKind = "valid"
    /\ phase' = "validated"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          faultKind, totalChunks, completedChunks, requestPublished>>

Reject ==
    /\ phase = "volume-admitted"
    /\ requestKind # "valid"
    /\ phase' = "rejected"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          faultKind, totalChunks, completedChunks, requestPublished>>

ReadMore ==
    /\ phase = "validated"
    /\ faultKind = "none"
    /\ brokerOutcome = "ok"
    /\ replyBinding = "exact"
    /\ completedChunks + 1 < totalChunks
    /\ phase' = "validated"
    /\ completedChunks' = completedChunks + 1
    /\ requestPublished' = TRUE
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          faultKind, totalChunks>>

Complete ==
    /\ phase = "validated"
    /\ faultKind = "none"
    /\ brokerOutcome = "ok"
    /\ replyBinding = "exact"
    /\ completedChunks + 1 = totalChunks
    /\ phase' = "complete"
    /\ completedChunks' = totalChunks
    /\ requestPublished' = TRUE
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          faultKind, totalChunks>>

RejectReply ==
    /\ phase = "validated"
    /\ faultKind = "none"
    /\ brokerOutcome = "ok"
    /\ replyBinding # "exact"
    /\ phase' = "reply-rejected"
    /\ requestPublished' = TRUE
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          faultKind, totalChunks, completedChunks>>

Timeout ==
    /\ phase = "validated"
    /\ faultKind = "none"
    /\ brokerOutcome = "timeout"
    /\ phase' = "timed-out"
    /\ requestPublished' = TRUE
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          faultKind, totalChunks, completedChunks>>

Unavailable ==
    /\ phase = "validated"
    /\ faultKind = "none"
    /\ brokerOutcome = "not-present"
    /\ phase' = "unavailable"
    /\ requestPublished' = TRUE
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          faultKind, totalChunks, completedChunks>>

DeviceFailure ==
    /\ phase = "validated"
    /\ faultKind = "none"
    /\ brokerOutcome = "device-fault"
    /\ phase' = "failed"
    /\ requestPublished' = TRUE
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          faultKind, totalChunks, completedChunks>>

InjectedFailure ==
    /\ phase = "validated"
    /\ faultKind # "none"
    /\ phase' = "failed"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          faultKind, totalChunks, completedChunks, requestPublished>>

Next ==
    \/ AdmitGeometry
    \/ RejectGeometry
    \/ AdmitImage
    \/ RejectImage
    \/ Accept
    \/ Reject
    \/ ReadMore
    \/ Complete
    \/ RejectReply
    \/ Timeout
    \/ Unavailable
    \/ DeviceFailure
    \/ InjectedFailure

TypeOK ==
    /\ phase \in {"idle", "geometry-admitted", "volume-admitted", "validated"}
                  \cup TerminalPhases
    /\ geometryKind \in GeometryKinds
    /\ imageKind \in ImageKinds
    /\ requestKind \in RequestKinds
    /\ brokerOutcome \in BrokerOutcomes
    /\ replyBinding \in ReplyBindings
    /\ faultKind \in FaultKinds
    /\ totalChunks \in 1..MaxChunks
    /\ completedChunks \in 0..MaxChunks
    /\ requestPublished \in BOOLEAN

InvalidRequestNeverDispatches ==
    requestKind # "valid" => phase # "validated"

InvalidGeometryNeverMounts ==
    geometryKind # "valid" => phase \notin {"volume-admitted", "validated", "complete"}

MalformedImageNeverMounts ==
    imageKind # "valid" => phase \notin {"volume-admitted", "validated", "complete"}

CompletionAccountsForEveryChunk ==
    phase = "complete" => completedChunks = totalChunks

MismatchedReplyNeverCompletes ==
    replyBinding # "exact" => phase # "complete"

TimeoutIsNotDeviceFailure ==
    brokerOutcome = "timeout" => phase # "failed"

UnavailableIsNotDeviceFailure ==
    brokerOutcome = "not-present" => phase # "failed"

ChunkCursorNeverExceedsRequest ==
    completedChunks <= totalChunks

ConfiguredFaultNeverPublishes ==
    faultKind # "none" => ~requestPublished

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

EventuallyTerminal == <> (phase \in TerminalPhases)
=============================================================================
