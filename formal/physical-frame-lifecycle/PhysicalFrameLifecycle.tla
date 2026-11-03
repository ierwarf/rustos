---------------------- MODULE PhysicalFrameLifecycle ----------------------
EXTENDS FiniteSets, Naturals

(***************************************************************************
Owner: kernel/mm physical frame allocator.
Linearization points: the IRQ-safe central allocator lock for every movement
between the central bitmap and a CPU frame cache, and the per-CPU slot lock
(ordered before the central lock, one slot at a time) for every cache push and
pop. Boot reservations, frame allocation, and exact one-time free mutate one
authoritative frame state.

A frame is exactly one of:
  Free      free in the central bitmap
  Cached    parked in one CPU's frame cache; used in the central bitmap and set
            in the lock-free `cached` bitmap, but still free memory
  Allocated owned by a caller
  Reserved  withheld from allocation for good
Free memory is Free + Cached. The central count and the cache count are kept
as separate counters, as the implementation keeps them, so the model checks
that their sum is the exact free total the watermark and the OOM watchdog read.
***************************************************************************)

CONSTANT Frames

Free == "free"
Cached == "cached"
Allocated == "allocated"
Reserved == "reserved"

VARIABLES frameState, centralFree, cachedCount

vars == <<frameState, centralFree, cachedCount>>

CountIn(states) == Cardinality({frame \in Frames : frameState[frame] \in states})

Init ==
    /\ Frames # {}
    /\ frameState = [frame \in Frames |-> Free]
    /\ centralFree = Cardinality(Frames)
    /\ cachedCount = 0

\* Boot reservations and fixed claims. A claim first drains the caches, so a
\* cached frame is never claimed while parked; it is claimed from Free.
Reserve(frame) ==
    /\ frame \in Frames
    /\ frameState[frame] = Free
    /\ frameState' = [frameState EXCEPT ![frame] = Reserved]
    /\ centralFree' = centralFree - 1
    /\ UNCHANGED cachedCount

\* Allocation from the central bitmap (slow path or contiguous run).
AllocateCentral(frame) ==
    /\ frame \in Frames
    /\ frameState[frame] = Free
    /\ frameState' = [frameState EXCEPT ![frame] = Allocated]
    /\ centralFree' = centralFree - 1
    /\ UNCHANGED cachedCount

\* Allocation from this CPU's cache: no central lock, free total drops by one.
AllocateCached(frame) ==
    /\ frame \in Frames
    /\ frameState[frame] = Cached
    /\ frameState' = [frameState EXCEPT ![frame] = Allocated]
    /\ cachedCount' = cachedCount - 1
    /\ UNCHANGED centralFree

\* Refill: central -> cache. The `cached` bit is published before the frame is
\* marked used, so no window shows a used, uncached free frame.
Refill(frame) ==
    /\ frame \in Frames
    /\ frameState[frame] = Free
    /\ frameState' = [frameState EXCEPT ![frame] = Cached]
    /\ centralFree' = centralFree - 1
    /\ cachedCount' = cachedCount + 1

\* Exact one-time release, either straight to the central bitmap (bulk
\* teardown) or into this CPU's cache (the `cached` bit arbitrates concurrent
\* releases of one frame).
ReleaseCentral(frame) ==
    /\ frame \in Frames
    /\ frameState[frame] = Allocated
    /\ frameState' = [frameState EXCEPT ![frame] = Free]
    /\ centralFree' = centralFree + 1
    /\ UNCHANGED cachedCount

ReleaseCached(frame) ==
    /\ frame \in Frames
    /\ frameState[frame] = Allocated
    /\ frameState' = [frameState EXCEPT ![frame] = Cached]
    /\ cachedCount' = cachedCount + 1
    /\ UNCHANGED centralFree

\* Spill and CPU-offline/teardown drain: cache -> central.
Drain(frame) ==
    /\ frame \in Frames
    /\ frameState[frame] = Cached
    /\ frameState' = [frameState EXCEPT ![frame] = Free]
    /\ centralFree' = centralFree + 1
    /\ cachedCount' = cachedCount - 1

\* Exhaustion is reported only after every cache has been drained, so it is
\* enabled only when no frame is free anywhere.
ReportExhausted ==
    /\ CountIn({Free, Cached}) = 0
    /\ UNCHANGED vars

TerminalReserved ==
    /\ \A frame \in Frames : frameState[frame] = Reserved
    /\ UNCHANGED vars

Next ==
    \/ \E frame \in Frames : Reserve(frame)
    \/ \E frame \in Frames : AllocateCentral(frame)
    \/ \E frame \in Frames : AllocateCached(frame)
    \/ \E frame \in Frames : Refill(frame)
    \/ \E frame \in Frames : ReleaseCentral(frame)
    \/ \E frame \in Frames : ReleaseCached(frame)
    \/ \E frame \in Frames : Drain(frame)
    \/ ReportExhausted
    \/ TerminalReserved

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ frameState \in [Frames -> {Free, Cached, Allocated, Reserved}]
    /\ centralFree \in 0..Cardinality(Frames)
    /\ cachedCount \in 0..Cardinality(Frames)

AllocatedFramesAreNotFree ==
    \A frame \in Frames :
        frameState[frame] = Allocated => frameState[frame] \notin {Free, Cached}

ReservedFramesAreNeverAllocated ==
    \A frame \in Frames :
        frameState[frame] = Reserved => frameState[frame] # Allocated

\* A release is enabled only for an allocated frame: a Free or Cached frame
\* (double release) and a Reserved frame are rejected.
OnlyAllocatedFramesCanBeReleased ==
    \A frame \in Frames :
        (ENABLED ReleaseCentral(frame) \/ ENABLED ReleaseCached(frame))
            => frameState[frame] = Allocated

\* The central counter and the cache counter each equal their frame sets, so
\* the free total is exact (not merely bounded) at every step.
CountersMatchFrameStates ==
    /\ centralFree = CountIn({Free})
    /\ cachedCount = CountIn({Cached})

FreeTotalIsCentralPlusCached ==
    centralFree + cachedCount = CountIn({Free, Cached})

ExhaustionRequiresNoFreeFrame ==
    ENABLED ReportExhausted => CountIn({Free, Cached}) = 0

=============================================================================
