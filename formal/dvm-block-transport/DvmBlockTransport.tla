--------------------------- MODULE DvmBlockTransport ---------------------------
EXTENDS Naturals, Sequences

(*******************************************************************************
Models the fixed RustOS/storage-DVM block queue and its durability boundary.

Concrete source contract:
  * libs/driver-domain-protocol/src/lib.rs
      DvmBlockHeader, DvmBlockRequest, DvmBlockCompletion

Integration owners:
  * kernel/io-manager signed-epoch transport frontend
  * Linux storage-DVM relay
  * services/storaged

The real wire records contain no address. A request selects only one fixed
host-owned data slot. This abstraction focuses on queue bounds, launch epochs,
restart/revoke, exact completion identity, and Virtio-compatible FUA/FLUSH
durability semantics.
*******************************************************************************)

CONSTANTS QueueDepth, MaxRequestId, MaxOperationId, MaxGeneration

NoRequest == [
    id |-> 0,
    epoch |-> 0,
    kind |-> "none",
    operationId |-> 0,
    fua |-> FALSE
]

RequestKinds == {"read", "write", "flush"}

SeqToSet(sequence) == {sequence[index] : index \in 1..Len(sequence)}

VARIABLES generation, epochSigned, forgedEpochRejected, dvmReady,
          nextRequestId, nextOperationId, requestQueue,
          active, completionQueue, durableThrough, staleEpoch,
          staleCompletionRejected, requestSignal, completionSignal,
          dvmArmed, hostArmed

vars == <<generation, epochSigned, forgedEpochRejected, dvmReady,
          nextRequestId, nextOperationId, requestQueue,
          active, completionQueue, durableThrough, staleEpoch,
          staleCompletionRejected, requestSignal, completionSignal,
          dvmArmed, hostArmed>>

Init ==
    /\ generation = 1
    /\ epochSigned = TRUE
    /\ forgedEpochRejected = FALSE
    /\ dvmReady = TRUE
    /\ nextRequestId = 1
    /\ nextOperationId = 1
    /\ requestQueue = <<>>
    /\ active = NoRequest
    /\ completionQueue = <<>>
    /\ durableThrough = 0
    /\ staleEpoch = 0
    /\ staleCompletionRejected = FALSE
    /\ requestSignal = FALSE
    /\ completionSignal = FALSE
    /\ dvmArmed = FALSE
    /\ hostArmed = FALSE

CanSubmit ==
    /\ dvmReady
    /\ Len(requestQueue) < QueueDepth
    /\ nextRequestId <= MaxRequestId

SubmitRead ==
    /\ CanSubmit
    /\ requestQueue' = Append(requestQueue, [
           id |-> nextRequestId,
           epoch |-> generation,
           kind |-> "read",
           operationId |-> 0,
           fua |-> FALSE
       ])
    /\ nextRequestId' = nextRequestId + 1
    /\ requestSignal' = TRUE
    /\ dvmArmed' = FALSE
    /\ UNCHANGED <<generation, epochSigned, forgedEpochRejected, dvmReady,
                   nextOperationId, active,
                   completionQueue, durableThrough, staleEpoch,
                   staleCompletionRejected, completionSignal, hostArmed>>

SubmitWrite(fua) ==
    /\ CanSubmit
    /\ nextOperationId <= MaxOperationId
    /\ requestQueue' = Append(requestQueue, [
           id |-> nextRequestId,
           epoch |-> generation,
           kind |-> "write",
           operationId |-> nextOperationId,
           fua |-> fua
       ])
    /\ nextRequestId' = nextRequestId + 1
    /\ nextOperationId' = nextOperationId + 1
    /\ requestSignal' = TRUE
    /\ dvmArmed' = FALSE
    /\ UNCHANGED <<generation, epochSigned, forgedEpochRejected, dvmReady,
                   active, completionQueue,
                   durableThrough, staleEpoch, staleCompletionRejected,
                   completionSignal, hostArmed>>

SubmitFlush ==
    /\ CanSubmit
    /\ nextOperationId <= MaxOperationId
    /\ requestQueue' = Append(requestQueue, [
           id |-> nextRequestId,
           epoch |-> generation,
           kind |-> "flush",
           operationId |-> nextOperationId,
           fua |-> FALSE
       ])
    /\ nextRequestId' = nextRequestId + 1
    /\ nextOperationId' = nextOperationId + 1
    /\ requestSignal' = TRUE
    /\ dvmArmed' = FALSE
    /\ UNCHANGED <<generation, epochSigned, forgedEpochRejected, dvmReady,
                   active, completionQueue,
                   durableThrough, staleEpoch, staleCompletionRejected,
                   completionSignal, hostArmed>>

DvmArm ==
    /\ Len(requestQueue) = 0
    /\ active = NoRequest
    /\ dvmArmed' = TRUE
    /\ UNCHANGED <<generation, epochSigned, forgedEpochRejected, dvmReady,
                   nextRequestId, nextOperationId,
                   requestQueue, active, completionQueue, durableThrough,
                   staleEpoch, staleCompletionRejected, requestSignal,
                   completionSignal, hostArmed>>

Consume ==
    /\ dvmReady
    /\ active = NoRequest
    /\ Len(requestQueue) > 0
    /\ active' = Head(requestQueue)
    /\ requestQueue' = Tail(requestQueue)
    /\ requestSignal' = FALSE
    /\ dvmArmed' = FALSE
    /\ UNCHANGED <<generation, epochSigned, forgedEpochRejected, dvmReady,
                   nextRequestId, nextOperationId,
                   completionQueue, durableThrough, staleEpoch,
                   staleCompletionRejected, completionSignal, hostArmed>>

CompletionReport(request) ==
    IF request.kind = "read"
    THEN 0
    ELSE IF request.kind = "flush" \/ (request.kind = "write" /\ request.fua)
    THEN request.operationId
    ELSE durableThrough

NextDurable(request) ==
    IF request.kind = "flush" \/ (request.kind = "write" /\ request.fua)
    THEN request.operationId
    ELSE durableThrough

Complete ==
    /\ dvmReady
    /\ active # NoRequest
    /\ active.epoch = generation
    /\ Len(completionQueue) < QueueDepth
    /\ completionQueue' = Append(completionQueue, [
           id |-> active.id,
           epoch |-> active.epoch,
           kind |-> active.kind,
           operationId |-> active.operationId,
           fua |-> active.fua,
           durable |-> CompletionReport(active)
       ])
    /\ durableThrough' = NextDurable(active)
    /\ active' = NoRequest
    /\ completionSignal' = TRUE
    /\ hostArmed' = FALSE
    /\ UNCHANGED <<generation, epochSigned, forgedEpochRejected, dvmReady,
                   nextRequestId, nextOperationId,
                   requestQueue, staleEpoch, staleCompletionRejected,
                   requestSignal, dvmArmed>>

HostArm ==
    /\ Len(completionQueue) = 0
    /\ hostArmed' = TRUE
    /\ UNCHANGED <<generation, epochSigned, forgedEpochRejected, dvmReady,
                   nextRequestId, nextOperationId,
                   requestQueue, active, completionQueue, durableThrough,
                   staleEpoch, staleCompletionRejected, requestSignal,
                   completionSignal, dvmArmed>>

ConsumeCompletion ==
    /\ Len(completionQueue) > 0
    /\ completionQueue' = Tail(completionQueue)
    /\ completionSignal' = FALSE
    /\ hostArmed' = FALSE
    /\ UNCHANGED <<generation, epochSigned, forgedEpochRejected, dvmReady,
                   nextRequestId, nextOperationId,
                   requestQueue, active, durableThrough, staleEpoch,
                   staleCompletionRejected, requestSignal, dvmArmed>>

CoalesceRequestSignal ==
    /\ requestSignal
    /\ requestSignal' = FALSE
    /\ UNCHANGED <<generation, epochSigned, forgedEpochRejected, dvmReady,
                   nextRequestId, nextOperationId,
                   requestQueue, active, completionQueue, durableThrough,
                   staleEpoch, staleCompletionRejected, completionSignal,
                   dvmArmed, hostArmed>>

CoalesceCompletionSignal ==
    /\ completionSignal
    /\ completionSignal' = FALSE
    /\ UNCHANGED <<generation, epochSigned, forgedEpochRejected, dvmReady,
                   nextRequestId, nextOperationId,
                   requestQueue, active, completionQueue, durableThrough,
                   staleEpoch, staleCompletionRejected, requestSignal,
                   dvmArmed, hostArmed>>

Restart ==
    /\ generation < MaxGeneration
    /\ staleEpoch' = generation
    /\ generation' = generation + 1
    /\ epochSigned' = FALSE
    /\ dvmReady' = FALSE
    /\ requestQueue' = <<>>
    /\ active' = NoRequest
    /\ completionQueue' = <<>>
    /\ durableThrough' = 0
    /\ nextRequestId' = 1
    /\ nextOperationId' = 1
    /\ requestSignal' = FALSE
    /\ completionSignal' = TRUE
    /\ dvmArmed' = FALSE
    /\ hostArmed' = FALSE
    /\ UNCHANGED <<forgedEpochRejected, staleCompletionRejected>>

HostSignEpoch ==
    /\ ~dvmReady
    /\ ~epochSigned
    /\ epochSigned' = TRUE
    /\ UNCHANGED <<generation, forgedEpochRejected, dvmReady,
                   nextRequestId, nextOperationId, requestQueue, active,
                   completionQueue, durableThrough, staleEpoch,
                   staleCompletionRejected, requestSignal, completionSignal,
                   dvmArmed, hostArmed>>

RejectForgedEpoch ==
    /\ ~dvmReady
    /\ ~epochSigned
    /\ forgedEpochRejected' = TRUE
    /\ UNCHANGED <<generation, epochSigned, dvmReady, nextRequestId,
                   nextOperationId, requestQueue, active, completionQueue,
                   durableThrough, staleEpoch, staleCompletionRejected,
                   requestSignal, completionSignal, dvmArmed, hostArmed>>

Readmit ==
    /\ ~dvmReady
    /\ epochSigned
    /\ dvmReady' = TRUE
    /\ UNCHANGED <<generation, epochSigned, forgedEpochRejected,
                   nextRequestId, nextOperationId, requestQueue,
                   active, completionQueue, durableThrough, staleEpoch,
                   staleCompletionRejected, requestSignal, completionSignal,
                   dvmArmed, hostArmed>>

RejectStaleCompletion ==
    /\ staleEpoch # 0
    /\ staleEpoch < generation
    /\ staleCompletionRejected' = TRUE
    /\ UNCHANGED <<generation, epochSigned, forgedEpochRejected, dvmReady,
                   nextRequestId, nextOperationId,
                   requestQueue, active, completionQueue, durableThrough,
                   staleEpoch, requestSignal, completionSignal, dvmArmed,
                   hostArmed>>

Next ==
    SubmitRead
    \/ SubmitWrite(FALSE)
    \/ SubmitWrite(TRUE)
    \/ SubmitFlush
    \/ DvmArm
    \/ Consume
    \/ Complete
    \/ HostArm
    \/ ConsumeCompletion
    \/ CoalesceRequestSignal
    \/ CoalesceCompletionSignal
    \/ Restart
    \/ HostSignEpoch
    \/ RejectForgedEpoch
    \/ Readmit
    \/ RejectStaleCompletion

RequestType(request) ==
    /\ request.id \in 0..MaxRequestId
    /\ request.epoch \in 0..MaxGeneration
    /\ request.kind \in RequestKinds \cup {"none"}
    /\ request.operationId \in 0..MaxOperationId
    /\ request.fua \in BOOLEAN

CompletionType(completion) ==
    /\ completion.id \in 1..MaxRequestId
    /\ completion.epoch \in 1..MaxGeneration
    /\ completion.kind \in RequestKinds
    /\ completion.operationId \in 0..MaxOperationId
    /\ completion.fua \in BOOLEAN
    /\ completion.durable \in 0..MaxOperationId

TypeOK ==
    /\ generation \in 1..MaxGeneration
    /\ epochSigned \in BOOLEAN
    /\ forgedEpochRejected \in BOOLEAN
    /\ dvmReady \in BOOLEAN
    /\ nextRequestId \in 1..(MaxRequestId + 1)
    /\ nextOperationId \in 1..(MaxOperationId + 1)
    /\ Len(requestQueue) \in 0..QueueDepth
    /\ \A request \in SeqToSet(requestQueue): RequestType(request)
    /\ RequestType(active)
    /\ Len(completionQueue) \in 0..QueueDepth
    /\ \A completion \in SeqToSet(completionQueue): CompletionType(completion)
    /\ durableThrough \in 0..MaxOperationId
    /\ staleEpoch \in 0..MaxGeneration
    /\ staleCompletionRejected \in BOOLEAN
    /\ requestSignal \in BOOLEAN
    /\ completionSignal \in BOOLEAN
    /\ dvmArmed \in BOOLEAN
    /\ hostArmed \in BOOLEAN

QueuesAreBounded ==
    /\ Len(requestQueue) <= QueueDepth
    /\ Len(completionQueue) <= QueueDepth

LiveRequestsBindCurrentEpoch ==
    /\ \A request \in SeqToSet(requestQueue): request.epoch = generation
    /\ (active # NoRequest => active.epoch = generation)

CompletionsBindCurrentEpoch ==
    \A completion \in SeqToSet(completionQueue):
        completion.epoch = generation

ReadsNeverInventDurability ==
    \A completion \in SeqToSet(completionQueue):
        completion.kind = "read" => completion.durable = 0

FuaAndFlushAreStable ==
    \A completion \in SeqToSet(completionQueue):
        (completion.kind = "flush" \/
         (completion.kind = "write" /\ completion.fua)) =>
            completion.durable = completion.operationId

DurabilityIsBoundedByAcceptedMutation ==
    durableThrough < nextOperationId

RestartRevokesOldQueues ==
    ~dvmReady =>
        /\ requestQueue = <<>>
        /\ active = NoRequest
        /\ completionQueue = <<>>

ReadyEpochIsHostSigned == dvmReady => epochSigned

NoSleeperMissesVisibleWork ==
    /\ (Len(requestQueue) > 0 => ~dvmArmed)
    /\ (Len(completionQueue) > 0 => ~hostArmed)

Spec == Init /\ [][Next]_vars
===============================================================================
