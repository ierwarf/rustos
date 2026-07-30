---------------------- MODULE SmpRescheduleIpi ----------------------
EXTENDS Naturals

(***************************************************************************
Models the per-CPU 0->1 reschedule request and fixed-IPI protocol.

The request bit is the durable work owner. The APIC pending bit is only a
notification. Receiving an IPI while a raw lock disables preemption may
acknowledge the interrupt, but must retain the request until a same-CPU safe
point consumes it. A safe dispatch consumes both and may change task owner.

Concrete owners:
  * kernel/ps/src/multitask/{cpu_local,irq}.rs
  * kernel/hal/src/arch/msi.rs
  * kernel/lowlevel/src/interrupts.rs
***************************************************************************)

CONSTANTS Cpus, MaxDispatchCount

VARIABLES online, request, ipiPending, preemptDisabled,
          deferredOutstanding, dispatchCount, unsafeDispatch

vars == <<online, request, ipiPending, preemptDisabled,
          deferredOutstanding, dispatchCount, unsafeDispatch>>

Init ==
    /\ online = [cpu \in Cpus |-> TRUE]
    /\ request = [cpu \in Cpus |-> FALSE]
    /\ ipiPending = [cpu \in Cpus |-> FALSE]
    /\ preemptDisabled = [cpu \in Cpus |-> FALSE]
    /\ deferredOutstanding = [cpu \in Cpus |-> FALSE]
    /\ dispatchCount = [cpu \in Cpus |-> 0]
    /\ unsafeDispatch = FALSE

PublishAndNotify(cpu) ==
    /\ online[cpu]
    /\ ~request[cpu]
    /\ request' = [request EXCEPT ![cpu] = TRUE]
    /\ ipiPending' = [ipiPending EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<online, preemptDisabled, deferredOutstanding,
                   dispatchCount, unsafeDispatch>>

Coalesce(cpu) ==
    /\ online[cpu]
    /\ request[cpu]
    /\ UNCHANGED vars

EnterCritical(cpu) ==
    /\ ~preemptDisabled[cpu]
    /\ preemptDisabled' = [preemptDisabled EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<online, request, ipiPending, deferredOutstanding,
                   dispatchCount, unsafeDispatch>>

ExitCritical(cpu) ==
    /\ preemptDisabled[cpu]
    /\ preemptDisabled' = [preemptDisabled EXCEPT ![cpu] = FALSE]
    /\ UNCHANGED <<online, request, ipiPending, deferredOutstanding,
                   dispatchCount, unsafeDispatch>>

ReceiveWhileLocked(cpu) ==
    /\ ipiPending[cpu]
    /\ preemptDisabled[cpu]
    /\ ipiPending' = [ipiPending EXCEPT ![cpu] = FALSE]
    /\ deferredOutstanding' =
        [deferredOutstanding EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<online, request, preemptDisabled,
                   dispatchCount, unsafeDispatch>>

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
    /\ UNCHANGED <<online, preemptDisabled>>

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
    /\ UNCHANGED <<online, preemptDisabled>>

Next ==
    \E cpu \in Cpus:
        \/ PublishAndNotify(cpu)
        \/ Coalesce(cpu)
        \/ EnterCritical(cpu)
        \/ ExitCritical(cpu)
        \/ ReceiveWhileLocked(cpu)
        \/ ReceiveAndDispatch(cpu)
        \/ ConsumeAtSafePoint(cpu)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ MaxDispatchCount \in Nat \ {0}
    /\ online \in [Cpus -> BOOLEAN]
    /\ request \in [Cpus -> BOOLEAN]
    /\ ipiPending \in [Cpus -> BOOLEAN]
    /\ preemptDisabled \in [Cpus -> BOOLEAN]
    /\ deferredOutstanding \in [Cpus -> BOOLEAN]
    \* Safety depends on whether and how often a consume edge was observed,
    \* not an unbounded lifetime total. Saturation keeps the exhaustive state
    \* space finite while distinguishing zero, one, and repeated dispatch.
    /\ dispatchCount \in [Cpus -> 0..MaxDispatchCount]
    /\ unsafeDispatch \in BOOLEAN

IpiRequiresDurableRequest ==
    \A cpu \in Cpus : ipiPending[cpu] => request[cpu]

DeferredReceiveNeverLosesRequest ==
    \A cpu \in Cpus : deferredOutstanding[cpu] => request[cpu]

NoDispatchWhilePreemptionDisabled ==
    ~unsafeDispatch

OfflineCpuOwnsNoNotification ==
    \A cpu \in Cpus : ~online[cpu] => ~ipiPending[cpu]

=============================================================================
