----------------------------- MODULE DvmVolumeIo -----------------------------
EXTENDS Naturals

(*******************************************************************************
Models the service-to-kernel storage-DVM I/O contract after a generation has
been admitted. The request must be non-empty, block aligned, and wholly
contained in the published volume before DVM dispatch. Chunk arithmetic may
not wrap, and recoverable broker failures retain their distinct outcome rather
than collapsing into a fabricated generic device fault.  The composed
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
BrokerOutcomes == {"ok", "timeout", "not-present", "device-fault"}
ReplyBindings == {"exact", "wrong-header", "wrong-generation", "wrong-range", "oversized"}
TerminalPhases ==
    {"rejected", "reply-rejected", "complete", "timed-out", "unavailable", "failed"}

VARIABLES phase, requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks

vars ==
    <<phase, requestKind, brokerOutcome, replyBinding,
      totalChunks, completedChunks>>

Init ==
    /\ phase = "idle"
    /\ requestKind \in RequestKinds
    /\ brokerOutcome \in BrokerOutcomes
    /\ replyBinding \in ReplyBindings
    /\ totalChunks \in 1..MaxChunks
    /\ completedChunks = 0

Accept ==
    /\ phase = "idle"
    /\ requestKind = "valid"
    /\ phase' = "validated"
    /\ UNCHANGED
        <<requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

Reject ==
    /\ phase = "idle"
    /\ requestKind # "valid"
    /\ phase' = "rejected"
    /\ UNCHANGED
        <<requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

ReadMore ==
    /\ phase = "validated"
    /\ brokerOutcome = "ok"
    /\ replyBinding = "exact"
    /\ completedChunks + 1 < totalChunks
    /\ phase' = "validated"
    /\ completedChunks' = completedChunks + 1
    /\ UNCHANGED <<requestKind, brokerOutcome, replyBinding, totalChunks>>

Complete ==
    /\ phase = "validated"
    /\ brokerOutcome = "ok"
    /\ replyBinding = "exact"
    /\ completedChunks + 1 = totalChunks
    /\ phase' = "complete"
    /\ completedChunks' = totalChunks
    /\ UNCHANGED <<requestKind, brokerOutcome, replyBinding, totalChunks>>

RejectReply ==
    /\ phase = "validated"
    /\ brokerOutcome = "ok"
    /\ replyBinding # "exact"
    /\ phase' = "reply-rejected"
    /\ UNCHANGED
        <<requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

Timeout ==
    /\ phase = "validated"
    /\ brokerOutcome = "timeout"
    /\ phase' = "timed-out"
    /\ UNCHANGED
        <<requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

Unavailable ==
    /\ phase = "validated"
    /\ brokerOutcome = "not-present"
    /\ phase' = "unavailable"
    /\ UNCHANGED
        <<requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

DeviceFailure ==
    /\ phase = "validated"
    /\ brokerOutcome = "device-fault"
    /\ phase' = "failed"
    /\ UNCHANGED
        <<requestKind, brokerOutcome, replyBinding,
          totalChunks, completedChunks>>

Next ==
    \/ Accept
    \/ Reject
    \/ ReadMore
    \/ Complete
    \/ RejectReply
    \/ Timeout
    \/ Unavailable
    \/ DeviceFailure

TypeOK ==
    /\ phase \in {"idle", "validated"} \cup TerminalPhases
    /\ requestKind \in RequestKinds
    /\ brokerOutcome \in BrokerOutcomes
    /\ replyBinding \in ReplyBindings
    /\ totalChunks \in 1..MaxChunks
    /\ completedChunks \in 0..MaxChunks

InvalidRequestNeverDispatches ==
    requestKind # "valid" => phase # "validated"

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
