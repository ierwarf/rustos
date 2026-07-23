--------------------------- MODULE BootVolumeRead ---------------------------
EXTENDS Naturals

(*******************************************************************************
Models the service-to-kernel boot-volume read contract after a volume has been
admitted. The request must be non-empty, block aligned, and wholly contained
in the published volume before any physical dispatch. Chunk arithmetic may
not wrap, and recoverable broker failures retain their distinct outcome rather
than collapsing into a fabricated generic device fault.

Concrete owners:
  * services/vfsd/src/block.rs
  * kernel/compat/src/user/syscall/linux/block_broker_ops.rs
  * kernel/io-manager/src/storage/block/io.rs
  * kernel/io-manager/src/storage/block/registry.rs
*******************************************************************************)

CONSTANT MaxChunks

RequestKinds == {"valid", "empty", "overflow", "overrun"}
BrokerOutcomes == {"ok", "timeout", "not-present", "device-fault"}
TerminalPhases == {"rejected", "complete", "timed-out", "unavailable", "failed"}

VARIABLES phase, requestKind, brokerOutcome, totalChunks, completedChunks

vars == <<phase, requestKind, brokerOutcome, totalChunks, completedChunks>>

Init ==
    /\ phase = "idle"
    /\ requestKind \in RequestKinds
    /\ brokerOutcome \in BrokerOutcomes
    /\ totalChunks \in 1..MaxChunks
    /\ completedChunks = 0

Accept ==
    /\ phase = "idle"
    /\ requestKind = "valid"
    /\ phase' = "validated"
    /\ UNCHANGED <<requestKind, brokerOutcome, totalChunks, completedChunks>>

Reject ==
    /\ phase = "idle"
    /\ requestKind # "valid"
    /\ phase' = "rejected"
    /\ UNCHANGED <<requestKind, brokerOutcome, totalChunks, completedChunks>>

ReadMore ==
    /\ phase = "validated"
    /\ brokerOutcome = "ok"
    /\ completedChunks + 1 < totalChunks
    /\ phase' = "validated"
    /\ completedChunks' = completedChunks + 1
    /\ UNCHANGED <<requestKind, brokerOutcome, totalChunks>>

Complete ==
    /\ phase = "validated"
    /\ brokerOutcome = "ok"
    /\ completedChunks + 1 = totalChunks
    /\ phase' = "complete"
    /\ completedChunks' = totalChunks
    /\ UNCHANGED <<requestKind, brokerOutcome, totalChunks>>

Timeout ==
    /\ phase = "validated"
    /\ brokerOutcome = "timeout"
    /\ phase' = "timed-out"
    /\ UNCHANGED <<requestKind, brokerOutcome, totalChunks, completedChunks>>

Unavailable ==
    /\ phase = "validated"
    /\ brokerOutcome = "not-present"
    /\ phase' = "unavailable"
    /\ UNCHANGED <<requestKind, brokerOutcome, totalChunks, completedChunks>>

DeviceFailure ==
    /\ phase = "validated"
    /\ brokerOutcome = "device-fault"
    /\ phase' = "failed"
    /\ UNCHANGED <<requestKind, brokerOutcome, totalChunks, completedChunks>>

Next ==
    \/ Accept
    \/ Reject
    \/ ReadMore
    \/ Complete
    \/ Timeout
    \/ Unavailable
    \/ DeviceFailure

TypeOK ==
    /\ phase \in {"idle", "validated"} \cup TerminalPhases
    /\ requestKind \in RequestKinds
    /\ brokerOutcome \in BrokerOutcomes
    /\ totalChunks \in 1..MaxChunks
    /\ completedChunks \in 0..MaxChunks

InvalidRequestNeverDispatches ==
    requestKind # "valid" => phase # "validated"

CompletionAccountsForEveryChunk ==
    phase = "complete" => completedChunks = totalChunks

TimeoutIsNotDeviceFailure ==
    brokerOutcome = "timeout" => phase # "failed"

UnavailableIsNotDeviceFailure ==
    brokerOutcome = "not-present" => phase # "failed"

ChunkCursorNeverExceedsRequest ==
    completedChunks <= totalChunks

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

EventuallyTerminal == <> (phase \in TerminalPhases)
=============================================================================
