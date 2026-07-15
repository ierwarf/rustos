------------------------- MODULE PageTableLifecycle --------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Owner: kernel-mm ProcessAddressSpace.
Linearization point: Map/Protect/Unmap page-table mutation plus the same-turn
TLB epoch increment. User mappings are bounded, frame-live and W^X.
*******************************************************************************)

CONSTANTS Pages, Frames, UserFrames, MaxEpoch

NoFrame == "none"
VARIABLES mapping, writable, executable, liveFrames, epoch
vars == <<mapping, writable, executable, liveFrames, epoch>>

Init ==
    /\ mapping = [p \in Pages |-> NoFrame]
    /\ writable = [p \in Pages |-> FALSE]
    /\ executable = [p \in Pages |-> FALSE]
    /\ liveFrames = UserFrames
    /\ epoch = 0

Map(p, f, w, x) ==
    /\ mapping[p] = NoFrame
    /\ f \in liveFrames
    /\ ~(w /\ x)
    /\ mapping' = [mapping EXCEPT ![p] = f]
    /\ writable' = [writable EXCEPT ![p] = w]
    /\ executable' = [executable EXCEPT ![p] = x]
    /\ epoch' = (epoch + 1) % (MaxEpoch + 1)
    /\ UNCHANGED liveFrames

Protect(p, w, x) ==
    /\ mapping[p] # NoFrame
    /\ ~(w /\ x)
    /\ writable' = [writable EXCEPT ![p] = w]
    /\ executable' = [executable EXCEPT ![p] = x]
    /\ epoch' = (epoch + 1) % (MaxEpoch + 1)
    /\ UNCHANGED <<mapping, liveFrames>>

Unmap(p) ==
    /\ mapping[p] # NoFrame
    /\ mapping' = [mapping EXCEPT ![p] = NoFrame]
    /\ writable' = [writable EXCEPT ![p] = FALSE]
    /\ executable' = [executable EXCEPT ![p] = FALSE]
    /\ epoch' = (epoch + 1) % (MaxEpoch + 1)
    /\ UNCHANGED liveFrames

Free(f) ==
    /\ f \in liveFrames
    /\ \A p \in Pages: mapping[p] # f
    /\ liveFrames' = liveFrames \ {f}
    /\ UNCHANGED <<mapping, writable, executable, epoch>>

Next ==
    \/ \E p \in Pages, f \in Frames, w \in BOOLEAN, x \in BOOLEAN: Map(p, f, w, x)
    \/ \E p \in Pages, w \in BOOLEAN, x \in BOOLEAN: Protect(p, w, x)
    \/ \E p \in Pages: Unmap(p)
    \/ \E f \in Frames: Free(f)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ mapping \in [Pages -> Frames \cup {NoFrame}]
    /\ writable \in [Pages -> BOOLEAN]
    /\ executable \in [Pages -> BOOLEAN]
    /\ liveFrames \in SUBSET UserFrames
    /\ epoch \in 0..MaxEpoch

MappedFramesAreLive == \A p \in Pages: mapping[p] # NoFrame => mapping[p] \in liveFrames
UserMappingsUseUserFrames == \A p \in Pages: mapping[p] # NoFrame => mapping[p] \in UserFrames
WritableXorExecutable == \A p \in Pages: ~(writable[p] /\ executable[p])
UnmappedHasNoAuthority ==
    \A p \in Pages: mapping[p] = NoFrame => ~writable[p] /\ ~executable[p]

=============================================================================
