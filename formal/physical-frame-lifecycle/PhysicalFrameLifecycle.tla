---------------------- MODULE PhysicalFrameLifecycle ----------------------
EXTENDS FiniteSets

(***************************************************************************
Owner: kernel/mm physical frame allocator.
Linearization point: the IRQ-safe allocator lock. Boot reservations, frame
allocation, and exact one-time free mutate one authoritative frame state.
***************************************************************************)

CONSTANT Frames

Free == "free"
Allocated == "allocated"
Reserved == "reserved"

VARIABLE frameState

vars == <<frameState>>

Init ==
    /\ Frames # {}
    /\ frameState = [frame \in Frames |-> Free]

Reserve(frame) ==
    /\ frame \in Frames
    /\ frameState[frame] = Free
    /\ frameState' = [frameState EXCEPT ![frame] = Reserved]

Allocate(frame) ==
    /\ frame \in Frames
    /\ frameState[frame] = Free
    /\ frameState' = [frameState EXCEPT ![frame] = Allocated]

Release(frame) ==
    /\ frame \in Frames
    /\ frameState[frame] = Allocated
    /\ frameState' = [frameState EXCEPT ![frame] = Free]

TerminalReserved ==
    /\ \A frame \in Frames : frameState[frame] = Reserved
    /\ UNCHANGED frameState

Next ==
    \/ \E frame \in Frames : Reserve(frame)
    \/ \E frame \in Frames : Allocate(frame)
    \/ \E frame \in Frames : Release(frame)
    \/ TerminalReserved

Spec == Init /\ [][Next]_vars

TypeOK == frameState \in [Frames -> {Free, Allocated, Reserved}]

AllocatedFramesAreNotFree ==
    \A frame \in Frames :
        frameState[frame] = Allocated => frameState[frame] # Free

ReservedFramesAreNeverAllocated ==
    \A frame \in Frames :
        frameState[frame] = Reserved => frameState[frame] # Allocated

OnlyAllocatedFramesCanBeReleased ==
    \A frame \in Frames :
        ENABLED Release(frame) => frameState[frame] = Allocated

=============================================================================
