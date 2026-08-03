---------------------- MODULE SmpRescheduleIpi ----------------------
EXTENDS Naturals

(***************************************************************************
Models the per-CPU 0->1 reschedule request, lock-held publication, post-unlock
fan-out, and fixed-IPI protocol.

The request bit is the durable work owner. The APIC pending bit is only a
notification. Receiving an IPI while a raw lock disables preemption may
acknowledge the interrupt, but must retain the request until a same-CPU safe
point consumes it. A safe dispatch consumes both and may change task owner.

Concrete owners:
  * kernel/ps/src/multitask/{cpu_local,irq}.rs
  * kernel/hal/src/arch/msi.rs
  * kernel/lowlevel/src/interrupts.rs
***************************************************************************)

CONSTANTS Cpus, NoCpu, MaxDispatchCount

VARIABLES online, request, ipiPending, preemptDisabled, fanoutPending, fanoutOwner,
          deferredOutstanding, dispatchCount, unsafeDispatch, selfIpiSent

vars == <<online, request, ipiPending, preemptDisabled, fanoutPending, fanoutOwner,
          deferredOutstanding, dispatchCount, unsafeDispatch, selfIpiSent>>

Init ==
    /\ online = [cpu \in Cpus |-> TRUE]
    /\ request = [cpu \in Cpus |-> FALSE]
    /\ ipiPending = [cpu \in Cpus |-> FALSE]
    /\ preemptDisabled = [cpu \in Cpus |-> FALSE]
    /\ fanoutPending = FALSE
    /\ fanoutOwner = NoCpu
    /\ deferredOutstanding = [cpu \in Cpus |-> FALSE]
    /\ dispatchCount = [cpu \in Cpus |-> 0]
    /\ unsafeDispatch = FALSE
    /\ selfIpiSent = FALSE

Publish(cpu) ==
    /\ online[cpu]
    /\ ~request[cpu] \/ ~fanoutPending
    /\ request' = [request EXCEPT ![cpu] = TRUE]
    /\ fanoutPending' = TRUE
    /\ fanoutOwner' = cpu
    /\ UNCHANGED <<online, ipiPending, preemptDisabled, deferredOutstanding,
                   dispatchCount, unsafeDispatch, selfIpiSent>>

\* A flusher may differ from the original publisher. Conservative fan-out
\* therefore covers every Online CPU; an already armed target retains its
\* request and needs no second IPI edge.
Fanout(flusher) ==
    /\ fanoutPending
    /\ online[flusher]
    /\ request' = [cpu \in Cpus |-> IF online[cpu] THEN TRUE ELSE request[cpu]]
    /\ ipiPending' =
        [cpu \in Cpus |->
            IF online[cpu] /\ cpu # flusher /\ ~request[cpu]
            THEN TRUE
            ELSE ipiPending[cpu]]
    /\ selfIpiSent' = FALSE
    /\ fanoutPending' = FALSE
    /\ fanoutOwner' = NoCpu
    /\ UNCHANGED <<online, preemptDisabled, deferredOutstanding,
                   dispatchCount, unsafeDispatch>>

FanoutAny == \E flusher \in Cpus: Fanout(flusher)

EnterCritical(cpu) ==
    /\ ~preemptDisabled[cpu]
    /\ preemptDisabled' = [preemptDisabled EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<online, request, ipiPending, fanoutPending, fanoutOwner, deferredOutstanding,
                   dispatchCount, unsafeDispatch, selfIpiSent>>

ExitCritical(cpu) ==
    /\ preemptDisabled[cpu]
    /\ preemptDisabled' = [preemptDisabled EXCEPT ![cpu] = FALSE]
    /\ UNCHANGED <<online, request, ipiPending, fanoutPending, fanoutOwner, deferredOutstanding,
                   dispatchCount, unsafeDispatch, selfIpiSent>>

ReceiveWhileLocked(cpu) ==
    /\ ipiPending[cpu]
    /\ preemptDisabled[cpu]
    /\ ipiPending' = [ipiPending EXCEPT ![cpu] = FALSE]
    /\ deferredOutstanding' =
        [deferredOutstanding EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<online, request, preemptDisabled, fanoutPending, fanoutOwner,
                   dispatchCount, unsafeDispatch, selfIpiSent>>

ReceiveAndDispatch(cpu) ==
    /\ ipiPending[cpu]
    /\ ~preemptDisabled[cpu]
    /\ request' = [request EXCEPT ![cpu] = FALSE]
    /\ ipiPending' = [ipiPending EXCEPT ![cpu] = FALSE]
    /\ deferredOutstanding' =
        [deferredOutstanding EXCEPT ![cpu] = FALSE]
    /\ dispatchCount' =
        [dispatchCount EXCEPT
            ![cpu] = IF @ < MaxDispatchCount THEN @ + 1 ELSE @]
    /\ unsafeDispatch' = unsafeDispatch \/ preemptDisabled[cpu]
    /\ UNCHANGED <<online, preemptDisabled, fanoutPending, fanoutOwner,
                   selfIpiSent>>

ConsumeAtSafePoint(cpu) ==
    /\ request[cpu]
    /\ ~preemptDisabled[cpu]
    /\ request' = [request EXCEPT ![cpu] = FALSE]
    /\ ipiPending' = [ipiPending EXCEPT ![cpu] = FALSE]
    /\ deferredOutstanding' =
        [deferredOutstanding EXCEPT ![cpu] = FALSE]
    /\ dispatchCount' =
        [dispatchCount EXCEPT
            ![cpu] = IF @ < MaxDispatchCount THEN @ + 1 ELSE @]
    /\ unsafeDispatch' = unsafeDispatch \/ preemptDisabled[cpu]
    /\ UNCHANGED <<online, preemptDisabled, fanoutPending, fanoutOwner,
                   selfIpiSent>>

Dispatch(cpu) == ReceiveAndDispatch(cpu) \/ ConsumeAtSafePoint(cpu)

Next ==
    \E cpu \in Cpus:
        \/ Publish(cpu)
        \/ EnterCritical(cpu)
        \/ ExitCritical(cpu)
        \/ ReceiveWhileLocked(cpu)
        \/ ReceiveAndDispatch(cpu)
        \/ ConsumeAtSafePoint(cpu)
    \/ FanoutAny

Spec ==
    Init /\ [][Next]_vars
    /\ WF_vars(FanoutAny)
    /\ (\A cpu \in Cpus: WF_vars(ExitCritical(cpu)))
    /\ (\A target \in Cpus: SF_vars(Dispatch(target)))

TypeOK ==
    /\ MaxDispatchCount \in Nat \ {0}
    /\ online \in [Cpus -> BOOLEAN]
    /\ request \in [Cpus -> BOOLEAN]
    /\ ipiPending \in [Cpus -> BOOLEAN]
    /\ preemptDisabled \in [Cpus -> BOOLEAN]
    /\ fanoutPending \in BOOLEAN
    /\ fanoutOwner \in Cpus \union {NoCpu}
    /\ NoCpu \notin Cpus
    /\ deferredOutstanding \in [Cpus -> BOOLEAN]
    \* Safety depends on whether and how often a consume edge was observed,
    \* not an unbounded lifetime total. Saturation keeps the exhaustive state
    \* space finite while distinguishing zero, one, and repeated dispatch.
    /\ dispatchCount \in [Cpus -> 0..MaxDispatchCount]
    /\ unsafeDispatch \in BOOLEAN
    /\ selfIpiSent \in BOOLEAN

IpiRequiresDurableRequest ==
    \A cpu \in Cpus : ipiPending[cpu] => request[cpu]

DeferredReceiveNeverLosesRequest ==
    \A cpu \in Cpus : deferredOutstanding[cpu] => request[cpu]

NoDispatchWhilePreemptionDisabled ==
    ~unsafeDispatch

NoSelfIpi == ~selfIpiSent

OfflineCpuOwnsNoNotification ==
    \A cpu \in Cpus : ~online[cpu] => ~ipiPending[cpu]

PendingFanoutHasDurableOwner ==
    /\ fanoutPending => fanoutOwner \in Cpus
    /\ ~fanoutPending => fanoutOwner = NoCpu

PendingFanoutEventuallyFlushes ==
    fanoutPending ~> ~fanoutPending

PublishedRequestEventuallyDispatches ==
    \A cpu \in Cpus: request[cpu] ~> ~request[cpu]

=============================================================================
