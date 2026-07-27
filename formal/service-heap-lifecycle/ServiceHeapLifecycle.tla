------------------------ MODULE ServiceHeapLifecycle ------------------------
EXTENDS FiniteSets

(***************************************************************************
Owner: rustos-svc-runtime userspace allocator.

The bootstrap heap and later mmap regions remain mapped for one service
generation.  Allocation transfers a free span to one live allocation; release
returns the exact span to the reusable set.  Capacity may grow only after the
currently mapped set has no free unit.  Allocation failure is explicit and
must not corrupt ownership.
***************************************************************************)

CONSTANTS Units, Threads

Unmapped == "unmapped"
Growing == "growing"
Free == "free"
Live == "live"
NoThread == "none"

VARIABLE unitState, allocationFailed, lockOwner, growthOwner

vars == <<unitState, allocationFailed, lockOwner, growthOwner>>

Init ==
    /\ Units # {}
    /\ Threads # {}
    /\ unitState = [unit \in Units |-> IF unit = CHOOSE u \in Units : TRUE
                                      THEN Free ELSE Unmapped]
    /\ allocationFailed = FALSE
    /\ lockOwner = NoThread
    /\ growthOwner = [unit \in Units |-> NoThread]

AcquireAllocator(thread) ==
    /\ ~allocationFailed
    /\ thread \in Threads
    /\ lockOwner = NoThread
    /\ ~(\E unit \in Units : growthOwner[unit] = thread)
    /\ lockOwner' = thread
    /\ UNCHANGED <<unitState, allocationFailed, growthOwner>>

Allocate(thread, unit) ==
    /\ ~allocationFailed
    /\ thread \in Threads
    /\ lockOwner = thread
    /\ unit \in Units
    /\ unitState[unit] = Free
    /\ unitState' = [unitState EXCEPT ![unit] = Live]
    /\ allocationFailed' = FALSE
    /\ lockOwner' = NoThread
    /\ UNCHANGED growthOwner

Release(thread, unit) ==
    /\ ~allocationFailed
    /\ thread \in Threads
    /\ lockOwner = thread
    /\ unit \in Units
    /\ unitState[unit] = Live
    /\ unitState' = [unitState EXCEPT ![unit] = Free]
    /\ lockOwner' = NoThread
    /\ UNCHANGED <<allocationFailed, growthOwner>>

RequestGrowth(thread, unit) ==
    /\ ~allocationFailed
    /\ thread \in Threads
    /\ lockOwner = thread
    /\ unit \in Units
    /\ unitState[unit] = Unmapped
    /\ ~(\E candidate \in Units : unitState[candidate] = Free)
    /\ unitState' = [unitState EXCEPT ![unit] = Growing]
    /\ growthOwner' = [growthOwner EXCEPT ![unit] = thread]
    /\ lockOwner' = NoThread
    /\ allocationFailed' = FALSE

FinishGrowth(thread, unit) ==
    /\ ~allocationFailed
    /\ thread \in Threads
    /\ unit \in Units
    /\ unitState[unit] = Growing
    /\ growthOwner[unit] = thread
    /\ lockOwner # thread
    /\ unitState' = [unitState EXCEPT ![unit] = Free]
    /\ growthOwner' = [growthOwner EXCEPT ![unit] = NoThread]
    /\ UNCHANGED <<allocationFailed, lockOwner>>

FailAllocation(thread) ==
    /\ ~allocationFailed
    /\ thread \in Threads
    /\ lockOwner = thread
    /\ ~(\E unit \in Units : unitState[unit] = Free)
    /\ ~(\E unit \in Units : unitState[unit] = Unmapped)
    /\ ~(\E unit \in Units : unitState[unit] = Growing)
    /\ allocationFailed' = TRUE
    /\ lockOwner' = NoThread
    /\ UNCHANGED <<unitState, growthOwner>>

TerminalFailure ==
    /\ allocationFailed
    /\ UNCHANGED vars

Next ==
    \/ \E thread \in Threads : AcquireAllocator(thread)
    \/ \E thread \in Threads, unit \in Units : Allocate(thread, unit)
    \/ \E thread \in Threads, unit \in Units : Release(thread, unit)
    \/ \E thread \in Threads, unit \in Units : RequestGrowth(thread, unit)
    \/ \E thread \in Threads, unit \in Units : FinishGrowth(thread, unit)
    \/ \E thread \in Threads : FailAllocation(thread)
    \/ TerminalFailure

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ unitState \in [Units -> {Unmapped, Growing, Free, Live}]
    /\ allocationFailed \in BOOLEAN
    /\ lockOwner \in Threads \cup {NoThread}
    /\ growthOwner \in [Units -> Threads \cup {NoThread}]

LiveIsNeverFree ==
    \A unit \in Units : unitState[unit] = Live => unitState[unit] # Free

ReleaseRequiresLiveOwnership ==
    \A thread \in Threads, unit \in Units :
        ENABLED Release(thread, unit) => unitState[unit] = Live

GrowthRequiresExhaustedMappedCapacity ==
    \A thread \in Threads, unit \in Units :
        ENABLED RequestGrowth(thread, unit) =>
            ~(\E candidate \in Units : unitState[candidate] = Free)

GrowingThreadNeverOwnsAllocatorLock ==
    \A unit \in Units :
        unitState[unit] = Growing => lockOwner # growthOwner[unit]

GrowingUnitsAreNotAllocatable ==
    \A unit \in Units :
        unitState[unit] = Growing => unitState[unit] # Free

FailurePreservesOwnership ==
    allocationFailed => ~(\E unit \in Units : unitState[unit] = Free)

=============================================================================
