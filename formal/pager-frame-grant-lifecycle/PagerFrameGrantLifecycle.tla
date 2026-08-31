--------------------- MODULE PagerFrameGrantLifecycle ---------------------
EXTENDS Naturals

(*******************************************************************************
Owner: kernel-mm frame-grant custody.
Linearization points: generation-bound publication and one exact consume or
cancel. The pager observes only an opaque handle; physical addresses never
cross the service boundary.
*******************************************************************************)

CONSTANTS Absent, Granted, Consumed, Cancelled, None, Grant, Mapping, Freed

VARIABLES state, generation, boundToken, frameCustody, consumeCount,
          acceptedBindingExact, acceptedGenerationExact, writable, executable,
          physicalExposed, wiredReserve, exceptionAllocatorTouched,
          frameRegistryLocked, leafPrepared, leafInstalled
vars == <<state, generation, boundToken, frameCustody, consumeCount,
          acceptedBindingExact, acceptedGenerationExact, writable, executable,
          physicalExposed, wiredReserve, exceptionAllocatorTouched,
          frameRegistryLocked, leafPrepared, leafInstalled>>

Init ==
    /\ state = Absent
    /\ generation = 0
    /\ boundToken = 0
    /\ frameCustody = None
    /\ consumeCount = 0
    /\ acceptedBindingExact = TRUE
    /\ acceptedGenerationExact = TRUE
    /\ writable = FALSE
    /\ executable = FALSE
    /\ physicalExposed = FALSE
    /\ wiredReserve = 1
    /\ exceptionAllocatorTouched = FALSE
    /\ frameRegistryLocked = FALSE
    /\ leafPrepared = FALSE
    /\ leafInstalled = FALSE

PrepareLeaf ==
    /\ state = Absent
    /\ ~leafInstalled
    /\ leafPrepared' = TRUE
    /\ UNCHANGED <<state, generation, boundToken, frameCustody, consumeCount,
                    acceptedBindingExact, acceptedGenerationExact, writable,
                    executable, physicalExposed, wiredReserve,
                    exceptionAllocatorTouched, frameRegistryLocked,
                    leafInstalled>>

Publish(token, mayWrite, mayExecute) ==
    /\ state = Absent
    /\ generation < 2
    /\ wiredReserve > 0
    /\ leafPrepared
    /\ token \in 1..2
    /\ ~(mayWrite /\ mayExecute)
    /\ state' = Granted
    /\ generation' = generation + 1
    /\ boundToken' = token
    /\ frameCustody' = Grant
    /\ consumeCount' = 0
    /\ acceptedBindingExact' = TRUE
    /\ acceptedGenerationExact' = TRUE
    /\ writable' = mayWrite
    /\ executable' = mayExecute
    /\ physicalExposed' = FALSE
    /\ wiredReserve' = wiredReserve - 1
    /\ exceptionAllocatorTouched' = FALSE
    /\ frameRegistryLocked' = FALSE
    /\ UNCHANGED <<leafPrepared, leafInstalled>>

RejectMismatch(token, handleGeneration) ==
    /\ state = Granted
    /\ \/ token # boundToken
       \/ handleGeneration # generation
    /\ UNCHANGED vars

Consume(token, handleGeneration) ==
    /\ state = Granted
    /\ token = boundToken
    /\ handleGeneration = generation
    /\ leafPrepared
    /\ state' = Consumed
    /\ frameCustody' = Mapping
    /\ consumeCount' = consumeCount + 1
    /\ acceptedBindingExact' = (token = boundToken)
    /\ acceptedGenerationExact' = (handleGeneration = generation)
    /\ leafInstalled' = TRUE
    /\ UNCHANGED <<generation, boundToken, writable, executable,
                    physicalExposed, wiredReserve, exceptionAllocatorTouched,
                    frameRegistryLocked, leafPrepared>>

Cancel(token, handleGeneration) ==
    /\ state = Granted
    /\ token = boundToken
    /\ handleGeneration = generation
    /\ state' = Cancelled
    /\ frameCustody' = Freed
    /\ acceptedBindingExact' = (token = boundToken)
    /\ acceptedGenerationExact' = (handleGeneration = generation)
    /\ wiredReserve' = wiredReserve + 1
    /\ UNCHANGED <<generation, boundToken, consumeCount, writable, executable,
                    physicalExposed, exceptionAllocatorTouched,
                    frameRegistryLocked, leafPrepared, leafInstalled>>

ReleaseMapping ==
    /\ state = Consumed
    /\ state' = Absent
    /\ frameCustody' = None
    /\ boundToken' = 0
    /\ writable' = FALSE
    /\ executable' = FALSE
    /\ leafInstalled' = FALSE
    /\ UNCHANGED <<generation, consumeCount, acceptedBindingExact,
                    acceptedGenerationExact, physicalExposed, wiredReserve,
                    exceptionAllocatorTouched, frameRegistryLocked,
                    leafPrepared>>

RecycleCancelled ==
    /\ state = Cancelled
    /\ state' = Absent
    /\ frameCustody' = None
    /\ boundToken' = 0
    /\ writable' = FALSE
    /\ executable' = FALSE
    /\ UNCHANGED <<generation, consumeCount, acceptedBindingExact,
                    acceptedGenerationExact, physicalExposed, wiredReserve,
                    exceptionAllocatorTouched, frameRegistryLocked,
                    leafPrepared, leafInstalled>>

ObserveTerminal ==
    /\ \/ state \in {Consumed, Cancelled}
       \/ /\ state = Absent
          /\ \/ generation = 2
             \/ wiredReserve = 0
    /\ UNCHANGED vars

Next ==
    \/ PrepareLeaf
    \/ \E token \in 1..2, mayWrite \in BOOLEAN, mayExecute \in BOOLEAN:
         Publish(token, mayWrite, mayExecute)
    \/ \E token \in 1..2, handleGeneration \in 1..2:
         RejectMismatch(token, handleGeneration)
    \/ \E token \in 1..2, handleGeneration \in 1..2:
         Consume(token, handleGeneration)
    \/ \E token \in 1..2, handleGeneration \in 1..2:
         Cancel(token, handleGeneration)
    \/ ReleaseMapping
    \/ RecycleCancelled
    \/ ObserveTerminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ state \in {Absent, Granted, Consumed, Cancelled}
    /\ generation \in 0..2
    /\ boundToken \in 0..2
    /\ frameCustody \in {None, Grant, Mapping, Freed}
    /\ consumeCount \in 0..1
    /\ acceptedBindingExact \in BOOLEAN
    /\ acceptedGenerationExact \in BOOLEAN
    /\ writable \in BOOLEAN
    /\ executable \in BOOLEAN
    /\ physicalExposed \in BOOLEAN
    /\ wiredReserve \in 0..1
    /\ exceptionAllocatorTouched \in BOOLEAN
    /\ frameRegistryLocked \in BOOLEAN
    /\ leafPrepared \in BOOLEAN
    /\ leafInstalled \in BOOLEAN

GrantHasExclusiveCustody == (state = Granted) => frameCustody = Grant
OneShotConsume == consumeCount <= 1
TransferRequiresExactBinding == (state = Consumed) => acceptedBindingExact
TransferRequiresExactGeneration == (state = Consumed) => acceptedGenerationExact
PublishedRightsAreWxSafe == (state = Granted) => ~(writable /\ executable)
PhysicalAddressNeverExposed == ~physicalExposed
FaultEntryNeverAllocatesOrLocks ==
    /\ ~exceptionAllocatorTouched
    /\ ~frameRegistryLocked
PublishedGrantRequiresPreparedLeaf == (state = Granted) => leafPrepared
ConsumedGrantInstallsPreparedLeaf ==
    (state = Consumed) => /\ leafPrepared /\ leafInstalled

=============================================================================
