---------------------- MODULE SmpRescheduleIpi ----------------------
EXTENDS Naturals

(***************************************************************************
The source uses one packed word per CPU: pending in bit zero and a monotonic
request sequence in the upper bits. Claiming pending and snapshotting its
sequence is one CAS. A producer that races after the claim must therefore
create a new pending edge; a producer that races before it is included in the
claimed sequence. Notification and dispatch are deliberately separate steps.
***************************************************************************)

CONSTANTS Cpus, MaxDispatchCount, MaxSequence, NoCpu

VARIABLES online, request, ipiPending, preemptDisabled, targetPending,
          targetCustodyOwed, deferredOutstanding, dispatchCount,
          unsafeDispatch, notifySource, lifecycleGen, requestSeq, notifySeq,
          claimedSeq, claimRoute, consumeSeq, lastRoute

vars == <<online, request, ipiPending, preemptDisabled, targetPending,
          targetCustodyOwed, deferredOutstanding, dispatchCount,
          unsafeDispatch, notifySource, lifecycleGen, requestSeq, notifySeq,
          claimedSeq, claimRoute, consumeSeq, lastRoute>>

Max(a, b) == IF a >= b THEN a ELSE b

Init ==
    /\ online = [cpu \in Cpus |-> TRUE]
    /\ request = [cpu \in Cpus |-> FALSE]
    /\ ipiPending = [cpu \in Cpus |-> FALSE]
    /\ preemptDisabled = [cpu \in Cpus |-> FALSE]
    /\ targetPending = [cpu \in Cpus |-> FALSE]
    /\ targetCustodyOwed = [cpu \in Cpus |-> FALSE]
    /\ deferredOutstanding = [cpu \in Cpus |-> FALSE]
    /\ dispatchCount = [cpu \in Cpus |-> 0]
    /\ unsafeDispatch = FALSE
    /\ notifySource = [cpu \in Cpus |-> NoCpu]
    /\ lifecycleGen = [cpu \in Cpus |-> 1]
    /\ requestSeq = [cpu \in Cpus |-> 0]
    /\ notifySeq = [cpu \in Cpus |-> 0]
    /\ claimedSeq = [cpu \in Cpus |-> 0]
    /\ claimRoute = [cpu \in Cpus |-> "none"]
    /\ consumeSeq = [cpu \in Cpus |-> 0]
    /\ lastRoute = [cpu \in Cpus |-> "none"]

PublishLocal(cpu) ==
    /\ online[cpu]
    /\ requestSeq[cpu] < MaxSequence
    /\ request' = [request EXCEPT ![cpu] = TRUE]
    /\ requestSeq' = [requestSeq EXCEPT ![cpu] = @ + 1]
    /\ UNCHANGED <<online, ipiPending, preemptDisabled, targetPending,
                   targetCustodyOwed, deferredOutstanding, dispatchCount,
                   unsafeDispatch, notifySource, lifecycleGen, notifySeq,
                   claimedSeq, claimRoute, consumeSeq, lastRoute>>

PublishTargetUnlocked(source, target) ==
    /\ online[source] /\ online[target]
    /\ ~preemptDisabled[source]
    /\ requestSeq[target] < MaxSequence
    /\ request' = [request EXCEPT ![target] = TRUE]
    /\ requestSeq' = [requestSeq EXCEPT ![target] = @ + 1]
    /\ ipiPending' = [ipiPending EXCEPT
          ![target] = IF ~request[target] /\ source # target THEN TRUE ELSE @]
    /\ notifySeq' = [notifySeq EXCEPT
          ![target] = IF ~request[target] /\ source # target
                       THEN requestSeq[target] + 1 ELSE @]
    /\ notifySource' = [notifySource EXCEPT
          ![target] = IF ~request[target] /\ source # target THEN source ELSE @]
    /\ UNCHANGED <<online, preemptDisabled, targetPending,
                   targetCustodyOwed, deferredOutstanding, dispatchCount,
                   unsafeDispatch, lifecycleGen, claimedSeq, claimRoute,
                   consumeSeq, lastRoute>>

PublishTargetLocked(source, target) ==
    /\ online[source] /\ online[target]
    /\ preemptDisabled[source]
    /\ requestSeq[target] < MaxSequence
    /\ request' = [request EXCEPT ![target] = TRUE]
    /\ requestSeq' = [requestSeq EXCEPT ![target] = @ + 1]
    /\ targetPending' = [targetPending EXCEPT
          ![target] = IF ~request[target] THEN TRUE ELSE @]
    /\ targetCustodyOwed' = [targetCustodyOwed EXCEPT
          ![target] = IF ~request[target] THEN TRUE ELSE @]
    /\ UNCHANGED <<online, ipiPending, preemptDisabled,
                   deferredOutstanding, dispatchCount, unsafeDispatch,
                   notifySource, lifecycleGen, notifySeq, claimedSeq,
                   claimRoute, consumeSeq, lastRoute>>

FlushTarget(flusher, target) ==
    /\ online[flusher] /\ online[target]
    /\ ~preemptDisabled[flusher]
    /\ targetPending[target]
    /\ targetPending' = [targetPending EXCEPT ![target] = FALSE]
    /\ targetCustodyOwed' = [targetCustodyOwed EXCEPT ![target] = FALSE]
    /\ ipiPending' = [ipiPending EXCEPT
          ![target] = IF request[target] /\ target # flusher THEN TRUE ELSE @]
    /\ notifySeq' = [notifySeq EXCEPT
          ![target] = IF request[target] /\ target # flusher
                       THEN requestSeq[target] ELSE @]
    /\ notifySource' = [notifySource EXCEPT
          ![target] = IF request[target] /\ target # flusher THEN flusher ELSE @]
    /\ UNCHANGED <<online, request, preemptDisabled, deferredOutstanding,
                   dispatchCount, unsafeDispatch, lifecycleGen, requestSeq,
                   claimedSeq, claimRoute, consumeSeq, lastRoute>>

FlushAny == \E flusher, target \in Cpus: FlushTarget(flusher, target)

EnterCritical(cpu) ==
    /\ ~preemptDisabled[cpu]
    /\ preemptDisabled' = [preemptDisabled EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<online, request, ipiPending, targetPending,
                   targetCustodyOwed, deferredOutstanding, dispatchCount,
                   unsafeDispatch, notifySource, lifecycleGen, requestSeq,
                   notifySeq, claimedSeq, claimRoute, consumeSeq, lastRoute>>

ExitCritical(cpu) ==
    /\ preemptDisabled[cpu]
    /\ preemptDisabled' = [preemptDisabled EXCEPT ![cpu] = FALSE]
    /\ UNCHANGED <<online, request, ipiPending, targetPending,
                   targetCustodyOwed, deferredOutstanding, dispatchCount,
                   unsafeDispatch, notifySource, lifecycleGen, requestSeq,
                   notifySeq, claimedSeq, claimRoute, consumeSeq, lastRoute>>

ReceiveWhileLocked(cpu) ==
    /\ ipiPending[cpu]
    /\ preemptDisabled[cpu]
    /\ ipiPending' = [ipiPending EXCEPT ![cpu] = FALSE]
    /\ deferredOutstanding' = [deferredOutstanding EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<online, request, preemptDisabled, targetPending,
                   targetCustodyOwed, dispatchCount, unsafeDispatch,
                   notifySource, lifecycleGen, requestSeq, notifySeq,
                   claimedSeq, claimRoute, consumeSeq, lastRoute>>

ClaimFromIpi(cpu) ==
    /\ ipiPending[cpu]
    /\ request[cpu]
    /\ ~preemptDisabled[cpu]
    /\ claimedSeq[cpu] = 0
    /\ request' = [request EXCEPT ![cpu] = FALSE]
    /\ ipiPending' = [ipiPending EXCEPT ![cpu] = FALSE]
    /\ deferredOutstanding' = [deferredOutstanding EXCEPT ![cpu] = FALSE]
    /\ claimedSeq' = [claimedSeq EXCEPT ![cpu] = requestSeq[cpu]]
    /\ claimRoute' = [claimRoute EXCEPT ![cpu] = "remote-ipi"]
    /\ notifySeq' = [notifySeq EXCEPT ![cpu] = Max(@, requestSeq[cpu])]
    /\ UNCHANGED <<online, preemptDisabled, targetPending,
                   targetCustodyOwed, dispatchCount, unsafeDispatch,
                   notifySource, lifecycleGen, requestSeq, consumeSeq, lastRoute>>

ClaimAtSafePoint(cpu) ==
    /\ request[cpu]
    /\ ~preemptDisabled[cpu]
    /\ claimedSeq[cpu] = 0
    /\ request' = [request EXCEPT ![cpu] = FALSE]
    /\ ipiPending' = [ipiPending EXCEPT ![cpu] = FALSE]
    /\ deferredOutstanding' = [deferredOutstanding EXCEPT ![cpu] = FALSE]
    /\ claimedSeq' = [claimedSeq EXCEPT ![cpu] = requestSeq[cpu]]
    /\ claimRoute' = [claimRoute EXCEPT ![cpu] = "local-safe-point"]
    /\ UNCHANGED <<online, preemptDisabled, targetPending,
                   targetCustodyOwed, dispatchCount, unsafeDispatch,
                   notifySource, lifecycleGen, requestSeq, notifySeq,
                   consumeSeq, lastRoute>>

DispatchClaim(cpu) ==
    /\ claimedSeq[cpu] > 0
    /\ ~preemptDisabled[cpu]
    /\ dispatchCount' = [dispatchCount EXCEPT
          ![cpu] = IF @ < MaxDispatchCount THEN @ + 1 ELSE @]
    /\ unsafeDispatch' = unsafeDispatch \/ preemptDisabled[cpu]
    /\ consumeSeq' = [consumeSeq EXCEPT ![cpu] = Max(@, claimedSeq[cpu])]
    /\ lastRoute' = [lastRoute EXCEPT ![cpu] = claimRoute[cpu]]
    /\ claimedSeq' = [claimedSeq EXCEPT ![cpu] = 0]
    /\ claimRoute' = [claimRoute EXCEPT ![cpu] = "none"]
    /\ UNCHANGED <<online, request, ipiPending, preemptDisabled,
                   targetPending, targetCustodyOwed, deferredOutstanding,
                   notifySource, lifecycleGen, requestSeq, notifySeq>>

Next ==
    \E cpu \in Cpus:
        \/ PublishLocal(cpu)
        \/ EnterCritical(cpu)
        \/ ExitCritical(cpu)
        \/ ReceiveWhileLocked(cpu)
        \/ ClaimFromIpi(cpu)
        \/ ClaimAtSafePoint(cpu)
        \/ DispatchClaim(cpu)
        \/ \E target \in Cpus:
            \/ PublishTargetUnlocked(cpu, target)
            \/ PublishTargetLocked(cpu, target)
    \/ FlushAny

Spec ==
    Init /\ [][Next]_vars
    /\ SF_vars(FlushAny)
    /\ (\A cpu \in Cpus: WF_vars(ExitCritical(cpu)))
    /\ (\A cpu \in Cpus: SF_vars(ClaimFromIpi(cpu) \/ ClaimAtSafePoint(cpu)))
    /\ (\A cpu \in Cpus: SF_vars(DispatchClaim(cpu)))

TypeOK ==
    /\ MaxDispatchCount \in Nat \ {0}
    /\ MaxSequence \in Nat \ {0}
    /\ online \in [Cpus -> BOOLEAN]
    /\ request \in [Cpus -> BOOLEAN]
    /\ ipiPending \in [Cpus -> BOOLEAN]
    /\ preemptDisabled \in [Cpus -> BOOLEAN]
    /\ targetPending \in [Cpus -> BOOLEAN]
    /\ targetCustodyOwed \in [Cpus -> BOOLEAN]
    /\ deferredOutstanding \in [Cpus -> BOOLEAN]
    /\ dispatchCount \in [Cpus -> 0..MaxDispatchCount]
    /\ unsafeDispatch \in BOOLEAN
    /\ notifySource \in [Cpus -> (Cpus \union {NoCpu})]
    /\ NoCpu \notin Cpus
    /\ lifecycleGen \in [Cpus -> Nat \ {0}]
    /\ requestSeq \in [Cpus -> 0..MaxSequence]
    /\ notifySeq \in [Cpus -> 0..MaxSequence]
    /\ claimedSeq \in [Cpus -> 0..MaxSequence]
    /\ claimRoute \in [Cpus -> {"none", "remote-ipi", "local-safe-point"}]
    /\ consumeSeq \in [Cpus -> 0..MaxSequence]
    /\ lastRoute \in [Cpus -> {"none", "remote-ipi", "local-safe-point"}]

ObservedSequencesAreBounded ==
    \A cpu \in Cpus:
        /\ consumeSeq[cpu] <= requestSeq[cpu]
        /\ notifySeq[cpu] <= requestSeq[cpu]
        /\ claimedSeq[cpu] <= requestSeq[cpu]

NoUnownedClearedRequest ==
    \A cpu \in Cpus:
        ~request[cpu] /\ claimedSeq[cpu] = 0 => consumeSeq[cpu] = requestSeq[cpu]

PostClaimPublicationCreatesNewEdge ==
    \A cpu \in Cpus:
        claimedSeq[cpu] > 0 /\ request[cpu] => requestSeq[cpu] > claimedSeq[cpu]

RemoteClaimIsNotified ==
    \A cpu \in Cpus:
        claimedSeq[cpu] > 0 /\ claimRoute[cpu] = "remote-ipi"
            => notifySeq[cpu] >= claimedSeq[cpu]

IpiRequiresDurableRequest ==
    \A cpu \in Cpus : ipiPending[cpu] => request[cpu]

PendingTargetHasAuthority ==
    \A cpu \in Cpus : targetPending[cpu] => targetCustodyOwed[cpu]

DeferredReceiveNeverLosesRequest ==
    \A cpu \in Cpus : deferredOutstanding[cpu] => request[cpu]

NoDispatchWhilePreemptionDisabled == ~unsafeDispatch
RemoteNotifyNeverTargetsFlusher ==
    \A cpu \in Cpus: notifySource[cpu] = NoCpu \/ notifySource[cpu] # cpu

PendingTargetEventuallyFlushes ==
    \A cpu \in Cpus: targetPending[cpu] ~> ~targetPending[cpu]

PublishedRequestEventuallyConsumed ==
    \A cpu \in Cpus: request[cpu] ~> (consumeSeq[cpu] = requestSeq[cpu])

=============================================================================
