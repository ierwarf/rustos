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
TerminalPhases ==
    {"geometry-rejected", "image-rejected", "rejected", "reply-rejected",
     "complete", "timed-out", "unavailable", "failed"}

VARIABLES phase, geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks

vars ==
    <<phase, geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
      totalChunks, completedChunks>>

Init ==
    /\ phase = "idle"
    /\ geometryKind \in GeometryKinds
    /\ imageKind \in ImageKinds
    /\ requestKind \in RequestKinds
    /\ brokerOutcome \in BrokerOutcomes
    /\ replyBinding \in ReplyBindings
    /\ totalChunks \in 1..MaxChunks
    /\ completedChunks = 0

AdmitGeometry ==
    /\ phase = "idle"
    /\ geometryKind = "valid"
    /\ phase' = "geometry-admitted"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

RejectGeometry ==
    /\ phase = "idle"
    /\ geometryKind # "valid"
    /\ phase' = "geometry-rejected"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

AdmitImage ==
    /\ phase = "geometry-admitted"
    /\ imageKind = "valid"
    /\ phase' = "volume-admitted"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

RejectImage ==
    /\ phase = "geometry-admitted"
    /\ imageKind # "valid"
    /\ phase' = "image-rejected"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

Accept ==
    /\ phase = "volume-admitted"
    /\ requestKind = "valid"
    /\ phase' = "validated"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

Reject ==
    /\ phase = "volume-admitted"
    /\ requestKind # "valid"
    /\ phase' = "rejected"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

ReadMore ==
    /\ phase = "validated"
    /\ brokerOutcome = "ok"
    /\ replyBinding = "exact"
    /\ completedChunks + 1 < totalChunks
    /\ phase' = "validated"
    /\ completedChunks' = completedChunks + 1
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding, totalChunks>>

Complete ==
    /\ phase = "validated"
    /\ brokerOutcome = "ok"
    /\ replyBinding = "exact"
    /\ completedChunks + 1 = totalChunks
    /\ phase' = "complete"
    /\ completedChunks' = totalChunks
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding, totalChunks>>

RejectReply ==
    /\ phase = "validated"
    /\ brokerOutcome = "ok"
    /\ replyBinding # "exact"
    /\ phase' = "reply-rejected"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

Timeout ==
    /\ phase = "validated"
    /\ brokerOutcome = "timeout"
    /\ phase' = "timed-out"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

Unavailable ==
    /\ phase = "validated"
    /\ brokerOutcome = "not-present"
    /\ phase' = "unavailable"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

DeviceFailure ==
    /\ phase = "validated"
    /\ brokerOutcome = "device-fault"
    /\ phase' = "failed"
    /\ UNCHANGED
        <<geometryKind, imageKind, requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

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

TypeOK ==
    /\ phase \in {"idle", "geometry-admitted", "volume-admitted", "validated"}
                  \cup TerminalPhases
    /\ geometryKind \in GeometryKinds
    /\ imageKind \in ImageKinds
    /\ requestKind \in RequestKinds
    /\ brokerOutcome \in BrokerOutcomes
    /\ replyBinding \in ReplyBindings
    /\ totalChunks \in 1..MaxChunks
    /\ completedChunks \in 0..MaxChunks

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

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

EventuallyTerminal == <> (phase \in TerminalPhases)
=============================================================================
