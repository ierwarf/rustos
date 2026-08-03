---------------------- MODULE SmpRescheduleIpi ----------------------
EXTENDS Naturals

(***************************************************************************
Models CPU-local reschedule requests, exact remote-target custody, lock-held
post-unlock notification, and the fixed-IPI protocol.

The request bit is the durable work owner. Ordinary local work never creates
remote authority. A remote publisher names one target; if its raw scheduler
owner disables preemption, an exact target-pending bit retains notification
custody until physical unlock. Receiving an IPI while a raw lock disables
preemption acknowledges the interrupt but retains the request until a same-CPU
safe point consumes it.

Concrete owners:
  * kernel/ps/src/multitask/{cpu_local,irq}.rs
  * kernel/ps/src/multitask/scheduler/{runqueue_policy,smp}.rs
  * kernel/hal/src/arch/msi.rs
  * kernel/lowlevel/src/interrupts.rs
***************************************************************************)

CONSTANTS Cpus, MaxDispatchCount, MaxSequence

VARIABLES online, request, ipiPending, preemptDisabled, targetPending,
          targetCustodyOwed, deferredOutstanding, dispatchCount,
          unsafeDispatch, selfIpiSent, lifecycleGen, requestSeq, notifySeq,
          consumeSeq, lastRoute

vars == <<online, request, ipiPending, preemptDisabled, targetPending,
          targetCustodyOwed, deferredOutstanding, dispatchCount,
          unsafeDispatch, selfIpiSent, lifecycleGen, requestSeq, notifySeq,
          consumeSeq, lastRoute>>

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
    /\ selfIpiSent = FALSE
    /\ lifecycleGen = [cpu \in Cpus |-> 1]
    /\ requestSeq = [cpu \in Cpus |-> 0]
    /\ notifySeq = [cpu \in Cpus |-> 0]
    /\ consumeSeq = [cpu \in Cpus |-> 0]
    /\ lastRoute = [cpu \in Cpus |-> "none"]

PublishLocal(cpu) ==
    /\ online[cpu]
    /\ requestSeq[cpu] < MaxSequence
    /\ request' = [request EXCEPT ![cpu] = TRUE]
    /\ requestSeq' = [requestSeq EXCEPT ![cpu] = @ + 1]
    /\ UNCHANGED <<online, ipiPending, preemptDisabled, targetPending,
                   targetCustodyOwed, deferredOutstanding, dispatchCount,
                   unsafeDispatch, selfIpiSent, lifecycleGen, notifySeq,
                   consumeSeq, lastRoute>>

PublishTargetUnlocked(source, target) ==
    /\ online[source] /\ online[target]
    /\ source # target
    /\ ~preemptDisabled[source]
    /\ requestSeq[target] < MaxSequence
    /\ request' = [request EXCEPT ![target] = TRUE]
    /\ requestSeq' = [requestSeq EXCEPT ![target] = @ + 1]
    /\ ipiPending' =
        [ipiPending EXCEPT ![target] = IF ~request[target] THEN TRUE ELSE @]
    /\ notifySeq' =
        [notifySeq EXCEPT
            ![target] = IF ~request[target] THEN requestSeq[target] + 1 ELSE @]
    /\ selfIpiSent' = FALSE
    /\ UNCHANGED <<online, preemptDisabled, targetPending,
                   targetCustodyOwed, deferredOutstanding, dispatchCount,
                   unsafeDispatch, lifecycleGen, consumeSeq, lastRoute>>

PublishTargetLocked(source, target) ==
    /\ online[source] /\ online[target]
    /\ source # target
    /\ preemptDisabled[source]
    /\ requestSeq[target] < MaxSequence
    /\ request' = [request EXCEPT ![target] = TRUE]
    /\ requestSeq' = [requestSeq EXCEPT ![target] = @ + 1]
    /\ targetPending' =
        [targetPending EXCEPT ![target] = IF ~request[target] THEN TRUE ELSE @]
    /\ targetCustodyOwed' =
        [targetCustodyOwed EXCEPT ![target] = IF ~request[target] THEN TRUE ELSE @]
    /\ UNCHANGED <<online, ipiPending, preemptDisabled, deferredOutstanding,
                   dispatchCount, unsafeDispatch, selfIpiSent, lifecycleGen,
                   notifySeq, consumeSeq, lastRoute>>

FlushTarget(flusher, target) ==
    /\ online[flusher] /\ online[target]
    /\ ~preemptDisabled[flusher]
    /\ targetPending[target]
    /\ request[target]
    /\ targetPending' = [targetPending EXCEPT ![target] = FALSE]
    /\ targetCustodyOwed' = [targetCustodyOwed EXCEPT ![target] = FALSE]
    /\ ipiPending' =
        [ipiPending EXCEPT ![target] = IF target # flusher THEN TRUE ELSE @]
    /\ notifySeq' =
        [notifySeq EXCEPT
            ![target] = IF target # flusher THEN requestSeq[target] ELSE @]
    /\ selfIpiSent' = FALSE
    /\ UNCHANGED <<online, request, preemptDisabled, deferredOutstanding,
                   dispatchCount, unsafeDispatch, lifecycleGen, requestSeq,
                   consumeSeq, lastRoute>>

FlushAny == \E flusher, target \in Cpus: FlushTarget(flusher, target)

EnterCritical(cpu) ==
    /\ ~preemptDisabled[cpu]
    /\ preemptDisabled' = [preemptDisabled EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<online, request, ipiPending, targetPending,
                   targetCustodyOwed, deferredOutstanding, dispatchCount,
                   unsafeDispatch, selfIpiSent, lifecycleGen, requestSeq,
                   notifySeq, consumeSeq, lastRoute>>

ExitCritical(cpu) ==
    /\ preemptDisabled[cpu]
    /\ preemptDisabled' = [preemptDisabled EXCEPT ![cpu] = FALSE]
    /\ UNCHANGED <<online, request, ipiPending, targetPending,
                   targetCustodyOwed, deferredOutstanding, dispatchCount,
                   unsafeDispatch, selfIpiSent, lifecycleGen, requestSeq,
                   notifySeq, consumeSeq, lastRoute>>

ReceiveWhileLocked(cpu) ==
    /\ ipiPending[cpu]
    /\ preemptDisabled[cpu]
    /\ ipiPending' = [ipiPending EXCEPT ![cpu] = FALSE]
    /\ deferredOutstanding' = [deferredOutstanding EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<online, request, preemptDisabled, targetPending,
                   targetCustodyOwed, dispatchCount, unsafeDispatch,
                   selfIpiSent, lifecycleGen, requestSeq, notifySeq,
                   consumeSeq, lastRoute>>

ReceiveAndDispatch(cpu) ==
    /\ ipiPending[cpu]
    /\ ~preemptDisabled[cpu]
    /\ ~targetPending[cpu]
    /\ request' = [request EXCEPT ![cpu] = FALSE]
    /\ ipiPending' = [ipiPending EXCEPT ![cpu] = FALSE]
    /\ deferredOutstanding' = [deferredOutstanding EXCEPT ![cpu] = FALSE]
    /\ dispatchCount' =
        [dispatchCount EXCEPT
            ![cpu] = IF @ < MaxDispatchCount THEN @ + 1 ELSE @]
    /\ unsafeDispatch' = unsafeDispatch \/ preemptDisabled[cpu]
    /\ consumeSeq' = [consumeSeq EXCEPT ![cpu] = requestSeq[cpu]]
    /\ lastRoute' = [lastRoute EXCEPT ![cpu] = "remote-ipi"]
    /\ UNCHANGED <<online, preemptDisabled, targetPending,
                   targetCustodyOwed, selfIpiSent, lifecycleGen, requestSeq,
                   notifySeq>>

ConsumeAtSafePoint(cpu) ==
    /\ request[cpu]
    /\ ~preemptDisabled[cpu]
    /\ ~targetPending[cpu]
    /\ request' = [request EXCEPT ![cpu] = FALSE]
    /\ ipiPending' = [ipiPending EXCEPT ![cpu] = FALSE]
    /\ deferredOutstanding' = [deferredOutstanding EXCEPT ![cpu] = FALSE]
    /\ dispatchCount' =
        [dispatchCount EXCEPT
            ![cpu] = IF @ < MaxDispatchCount THEN @ + 1 ELSE @]
    /\ unsafeDispatch' = unsafeDispatch \/ preemptDisabled[cpu]
    /\ consumeSeq' = [consumeSeq EXCEPT ![cpu] = requestSeq[cpu]]
    /\ lastRoute' = [lastRoute EXCEPT ![cpu] = "local-safe-point"]
    /\ UNCHANGED <<online, preemptDisabled, targetPending,
                   targetCustodyOwed, selfIpiSent, lifecycleGen, requestSeq,
                   notifySeq>>

Dispatch(cpu) == ReceiveAndDispatch(cpu) \/ ConsumeAtSafePoint(cpu)

Next ==
    \E cpu \in Cpus:
        \/ PublishLocal(cpu)
        \/ EnterCritical(cpu)
        \/ ExitCritical(cpu)
        \/ ReceiveWhileLocked(cpu)
        \/ ReceiveAndDispatch(cpu)
        \/ ConsumeAtSafePoint(cpu)
        \/ \E target \in Cpus:
            \/ PublishTargetUnlocked(cpu, target)
            \/ PublishTargetLocked(cpu, target)
    \/ FlushAny

Spec ==
    Init /\ [][Next]_vars
    /\ SF_vars(FlushAny)
    /\ (\A cpu \in Cpus: WF_vars(ExitCritical(cpu)))
    /\ (\A target \in Cpus: SF_vars(Dispatch(target)))

TypeOK ==
    /\ MaxDispatchCount \in Nat \ {0}
    /\ online \in [Cpus -> BOOLEAN]
    /\ request \in [Cpus -> BOOLEAN]
    /\ ipiPending \in [Cpus -> BOOLEAN]
    /\ preemptDisabled \in [Cpus -> BOOLEAN]
    /\ targetPending \in [Cpus -> BOOLEAN]
    /\ targetCustodyOwed \in [Cpus -> BOOLEAN]
    /\ deferredOutstanding \in [Cpus -> BOOLEAN]
    /\ dispatchCount \in [Cpus -> 0..MaxDispatchCount]
    /\ unsafeDispatch \in BOOLEAN
    /\ selfIpiSent \in BOOLEAN
    /\ MaxSequence \in Nat \ {0}
    /\ lifecycleGen \in [Cpus -> Nat \ {0}]
    /\ requestSeq \in [Cpus -> 0..MaxSequence]
    /\ notifySeq \in [Cpus -> 0..MaxSequence]
    /\ consumeSeq \in [Cpus -> 0..MaxSequence]
    /\ lastRoute \in [Cpus -> {"none", "remote-ipi", "local-safe-point"}]

ObservedSequencesAreBounded ==
    \A cpu \in Cpus:
        /\ consumeSeq[cpu] <= requestSeq[cpu]
        /\ notifySeq[cpu] <= requestSeq[cpu]

ClearedRequestWasConsumed ==
    \A cpu \in Cpus: ~request[cpu] => consumeSeq[cpu] = requestSeq[cpu]

IpiRequiresDurableRequest ==
    \A cpu \in Cpus : ipiPending[cpu] => request[cpu]

PendingTargetHasDurableRequest ==
    \A cpu \in Cpus : targetPending[cpu] => request[cpu]

LockedTargetCustodyIsNeverLost ==
    \A cpu \in Cpus : targetCustodyOwed[cpu] => targetPending[cpu]

DeferredReceiveNeverLosesRequest ==
    \A cpu \in Cpus : deferredOutstanding[cpu] => request[cpu]

NoDispatchWhilePreemptionDisabled == ~unsafeDispatch

NoSelfIpi == ~selfIpiSent

OfflineCpuOwnsNoNotification ==
    \A cpu \in Cpus : ~online[cpu] => ~ipiPending[cpu] /\ ~targetPending[cpu]

PendingTargetEventuallyFlushes ==
    \A cpu \in Cpus: targetPending[cpu] ~> ~targetPending[cpu]

PublishedRequestEventuallyDispatches ==
    \A cpu \in Cpus: request[cpu] ~> ~request[cpu]

=============================================================================
