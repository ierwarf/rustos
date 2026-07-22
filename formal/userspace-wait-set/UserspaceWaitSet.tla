--------------------------- MODULE UserspaceWaitSet ---------------------------
EXTENDS Naturals

(*******************************************************************************
Finite model of the generic RustOS userspace readiness wait set.

Concrete owners:
  * vfsd owns epoll interests and its control generation.
  * netd/inputd own consumable readiness and monotonic generations.
  * ring0 owns only bounded task wait tokens, deadline wakeup, and the
    check-register-provider-recheck-scheduler-arm-presence-check substrate.

The model combines the high-risk races: a provider transition between check
and sleep, timeout competing with a signal, provider restart/revoke, explicit
MOD rebind of a stable registration identity to the new provider epoch, and
open-description references inherited by dup/fork then retired by close/exec.
The terminal revoked outcome refines to per-interest ERR/HUP; unrelated ready
interests in the same aggregate wait remain reportable.
DuplicateObject/ForkObject abstract a concrete two-phase commit that rechecks
the source handle token under the fd-table lock and retires the exact replaced
target before publishing the new descriptor.
ArmRecheck abstracts the concrete final provider recheck plus waiter-presence
test; service IPC completes before the scheduler arm and is independently
bounded by the ipc-reply-deadline contract and the application deadline. A
provider-query timeout is an allowed nonterminal stutter/retry: it cannot erase
readiness found elsewhere in the scan or create an early terminal timeout.
*******************************************************************************)

CONSTANTS MaxGeneration, MaxEpoch, MaxTime, WaitBound, MaxRefs, MaxIngress

WaitStates == {"idle", "armed", "sleeping", "woken",
               "returned-ready", "returned-timeout", "returned-revoked",
               "returned-interrupted"}
TerminalStates == {"returned-ready", "returned-timeout", "returned-revoked",
                   "returned-interrupted"}

VARIABLES generation, epoch, providerLive, ready, waitState, observedGeneration,
          observedEpoch, deadline, now, objectRefs, epollRefs, ingressBacklog

vars == <<generation, epoch, providerLive, ready, waitState, observedGeneration,
          observedEpoch, deadline, now, objectRefs, epollRefs, ingressBacklog>>

Init ==
    /\ generation = 1
    /\ epoch = 1
    /\ providerLive = TRUE
    /\ ready = FALSE
    /\ waitState = "idle"
    /\ observedGeneration = 1
    /\ observedEpoch = 1
    /\ deadline = 0
    /\ now = 0
    /\ objectRefs = 1
    /\ epollRefs = 1
    /\ ingressBacklog = 0

BeginWait ==
    /\ waitState = "idle"
    /\ epollRefs > 0
    /\ now + WaitBound <= MaxTime
    /\ observedGeneration' = generation
    /\ deadline' = now + WaitBound
    /\ waitState' = IF ~providerLive \/ epoch # observedEpoch
                     THEN "returned-revoked"
                     ELSE IF ready THEN "returned-ready" ELSE "armed"
    /\ UNCHANGED <<generation, epoch, providerLive, ready, observedEpoch, now,
                   objectRefs, epollRefs, ingressBacklog>>

ExternalIngress ==
    /\ providerLive
    /\ ingressBacklog < MaxIngress
    /\ waitState \notin TerminalStates
    /\ ingressBacklog' = ingressBacklog + 1
    /\ UNCHANGED <<generation, epoch, providerLive, ready, waitState,
                   observedGeneration, observedEpoch, deadline, now, objectRefs,
                   epollRefs>>

IngressStep ==
    /\ providerLive
    /\ objectRefs > 0
    /\ ingressBacklog > 0
    /\ generation < MaxGeneration
    /\ waitState \notin TerminalStates
    /\ ingressBacklog' = ingressBacklog - 1
    /\ generation' = generation + 1
    /\ ready' = TRUE
    /\ waitState' = IF waitState \in {"armed", "sleeping"}
                     THEN "woken" ELSE waitState
    /\ UNCHANGED <<epoch, providerLive, observedGeneration, observedEpoch,
                   deadline, now, objectRefs, epollRefs>>

ProviderReady ==
    /\ providerLive
    /\ objectRefs > 0
    /\ generation < MaxGeneration
    /\ waitState \notin TerminalStates
    /\ generation' = generation + 1
    /\ ready' = TRUE
    /\ waitState' = IF waitState \in {"armed", "sleeping"}
                     THEN "woken" ELSE waitState
    /\ UNCHANGED <<epoch, providerLive, observedGeneration, observedEpoch,
                   deadline, now, objectRefs, epollRefs, ingressBacklog>>

ConsumeReady ==
    /\ providerLive
    /\ ready
    /\ generation < MaxGeneration
    /\ waitState \notin TerminalStates
    /\ generation' = generation + 1
    /\ ready' = FALSE
    /\ waitState' = IF waitState \in {"armed", "sleeping"}
                     THEN "woken" ELSE waitState
    /\ UNCHANGED <<epoch, providerLive, observedGeneration, observedEpoch,
                   deadline, now, objectRefs, epollRefs, ingressBacklog>>

ArmRecheck ==
    /\ waitState = "armed"
    /\ waitState' = IF ~providerLive \/ epoch # observedEpoch \/
                         ready \/ generation # observedGeneration
                     THEN "woken" ELSE "sleeping"
    /\ UNCHANGED <<generation, epoch, providerLive, ready, observedGeneration,
                   observedEpoch, deadline, now, objectRefs, epollRefs,
                   ingressBacklog>>

TimeoutWake ==
    /\ waitState \in {"armed", "sleeping"}
    /\ now >= deadline
    /\ waitState' = "woken"
    /\ UNCHANGED <<generation, epoch, providerLive, ready, observedGeneration,
                   observedEpoch, deadline, now, objectRefs, epollRefs,
                   ingressBacklog>>

SignalCancel ==
    /\ waitState \in {"armed", "sleeping", "woken"}
    /\ waitState' = "returned-interrupted"
    /\ UNCHANGED <<generation, epoch, providerLive, ready, observedGeneration,
                   observedEpoch, deadline, now, objectRefs, epollRefs,
                   ingressBacklog>>

ResolveWake ==
    /\ waitState = "woken"
    /\ observedGeneration' = generation
    /\ waitState' = IF ~providerLive \/ epoch # observedEpoch \/ epollRefs = 0
                     THEN "returned-revoked"
                     ELSE IF ready THEN "returned-ready"
                     ELSE IF now >= deadline THEN "returned-timeout"
                     ELSE "armed"
    /\ UNCHANGED <<generation, epoch, providerLive, ready, observedEpoch,
                   deadline, now, objectRefs, epollRefs, ingressBacklog>>

ProviderRestart ==
    /\ providerLive
    /\ epoch < MaxEpoch
    /\ waitState \notin TerminalStates
    /\ providerLive' = FALSE
    /\ epoch' = epoch + 1
    /\ generation' = 1
    /\ ready' = FALSE
    /\ waitState' = IF waitState \in {"armed", "sleeping", "woken"}
                     THEN "woken" ELSE waitState
    /\ UNCHANGED <<observedGeneration, observedEpoch, deadline, now, objectRefs,
                   epollRefs, ingressBacklog>>

ProviderRecover ==
    /\ ~providerLive
    /\ waitState \notin TerminalStates
    /\ providerLive' = TRUE
    /\ UNCHANGED <<generation, epoch, ready, waitState, observedGeneration,
                   observedEpoch, deadline, now, objectRefs, epollRefs,
                   ingressBacklog>>

DuplicateObject ==
    /\ objectRefs > 0
    /\ objectRefs < MaxRefs
    /\ waitState \notin TerminalStates
    /\ objectRefs' = objectRefs + 1
    /\ UNCHANGED <<generation, epoch, providerLive, ready, waitState,
                   observedGeneration, observedEpoch, deadline, now, epollRefs,
                   ingressBacklog>>

\* Concrete fork acquires provider refs from the same frozen HandleTable clone
\* that is published in the child, never from a later live-parent resnapshot.
ForkObject == DuplicateObject

CloseObject ==
    /\ objectRefs > 0
    /\ generation < MaxGeneration
    /\ waitState \notin TerminalStates
    /\ objectRefs' = objectRefs - 1
    /\ generation' = generation + 1
    /\ ready' = IF objectRefs = 1 THEN FALSE ELSE ready
    /\ waitState' = IF waitState \in {"armed", "sleeping"}
                     THEN "woken" ELSE waitState
    /\ UNCHANGED <<epoch, providerLive, observedGeneration, observedEpoch,
                   deadline, now, epollRefs, ingressBacklog>>

\* Concrete exec returns the exact CLOEXEC handles retired by the atomic
\* process-table replacement; provider cleanup is derived from that list.
ExecCloseObject == CloseObject

DuplicateEpoll ==
    /\ epollRefs > 0
    /\ epollRefs < MaxRefs
    /\ waitState \notin TerminalStates
    /\ epollRefs' = epollRefs + 1
    /\ UNCHANGED <<generation, epoch, providerLive, ready, waitState,
                   observedGeneration, observedEpoch, deadline, now, objectRefs,
                   ingressBacklog>>

CloseEpoll ==
    /\ epollRefs > 0
    /\ waitState \notin TerminalStates
    /\ epollRefs' = epollRefs - 1
    /\ waitState' = IF epollRefs = 1 /\
                         waitState \in {"armed", "sleeping", "woken"}
                     THEN "woken" ELSE waitState
    /\ UNCHANGED <<generation, epoch, providerLive, ready, observedGeneration,
                   observedEpoch, deadline, now, objectRefs, ingressBacklog>>

Tick ==
    /\ now < MaxTime
    /\ waitState \notin TerminalStates
    /\ now' = now + 1
    /\ UNCHANGED <<generation, epoch, providerLive, ready, waitState,
                   observedGeneration, observedEpoch, deadline, objectRefs,
                   epollRefs, ingressBacklog>>

\* Concrete ADD/MOD pins a still-live target open description across the vfsd
\* mutation. Its final guard release performs the same last-close purge, so a
\* purge-before-ADD race cannot recreate a retired interest. MOD retains the
\* target-fd/open-description key and replaces only the stale endpoint epoch.
\* ADD cannot create a second key for the same registration, and DEL uses that
\* stable key regardless of epoch.
ModifyInterestEpoch ==
    /\ waitState = "idle"
    /\ providerLive
    /\ objectRefs > 0
    /\ epoch # observedEpoch
    /\ observedEpoch' = epoch
    /\ UNCHANGED <<generation, epoch, providerLive, ready, waitState,
                   observedGeneration, deadline, now, objectRefs, epollRefs,
                   ingressBacklog>>

ResetWait ==
    /\ waitState \in TerminalStates
    /\ waitState' = "idle"
    /\ deadline' = 0
    /\ UNCHANGED <<generation, epoch, providerLive, ready, observedGeneration,
                   observedEpoch, now, objectRefs, epollRefs, ingressBacklog>>

TerminalStutter ==
    /\ waitState \in TerminalStates
    /\ UNCHANGED vars

Next ==
    \/ BeginWait
    \/ ExternalIngress
    \/ IngressStep
    \/ ProviderReady
    \/ ConsumeReady
    \/ ArmRecheck
    \/ TimeoutWake
    \/ SignalCancel
    \/ ResolveWake
    \/ ProviderRestart
    \/ ProviderRecover
    \/ ModifyInterestEpoch
    \/ ResetWait
    \/ DuplicateObject
    \/ ForkObject
    \/ CloseObject
    \/ ExecCloseObject
    \/ DuplicateEpoll
    \/ CloseEpoll
    \/ Tick
    \/ TerminalStutter

Spec == Init /\ [][Next]_vars
        /\ WF_vars(Tick)
        /\ WF_vars(IngressStep)
        /\ WF_vars(ArmRecheck)
        /\ WF_vars(TimeoutWake)
        /\ WF_vars(ResolveWake)

TypeOK ==
    /\ generation \in 1..MaxGeneration
    /\ epoch \in 1..MaxEpoch
    /\ providerLive \in BOOLEAN
    /\ ready \in BOOLEAN
    /\ waitState \in WaitStates
    /\ observedGeneration \in 1..MaxGeneration
    /\ observedEpoch \in 1..MaxEpoch
    /\ deadline \in 0..MaxTime
    /\ now \in 0..MaxTime
    /\ objectRefs \in 0..MaxRefs
    /\ epollRefs \in 0..MaxRefs
    /\ ingressBacklog \in 0..MaxIngress

SleepingRequiresStableRecheck ==
    waitState = "sleeping" =>
        providerLive /\ ~ready /\ generation = observedGeneration /\
        epoch = observedEpoch /\ epollRefs > 0

ReadyReturnIsAuthoritative ==
    waitState = "returned-ready" =>
        providerLive /\ ready /\ epoch = observedEpoch /\ epollRefs > 0

RevokedProviderCannotReturnReady ==
    (~providerLive \/ epoch # observedEpoch \/ epollRefs = 0) =>
        waitState # "returned-ready"

ReferenceCountsNeverUnderflow == objectRefs >= 0 /\ epollRefs >= 0

ClosedDescriptionCannotRegainReadiness == objectRefs = 0 => ~ready

ActiveWaitEventuallySettles ==
    waitState \in {"armed", "sleeping", "woken"} ~> waitState \in TerminalStates

AdmittedIngressEventuallySettlesOrExhausts ==
    ingressBacklog > 0 /\ providerLive /\ objectRefs > 0 /\
      generation < MaxGeneration
    ~> ready \/ ~providerLive \/ objectRefs = 0 \/
       generation = MaxGeneration \/ waitState \in TerminalStates

=============================================================================
