----------------- MODULE CapabilityDerivationLifecycle -----------------
EXTENDS FiniteSets, Naturals

(*******************************************************************************
One parent capability may derive one attenuated child. Object generation,
provider lease epoch, and derivation revoke epoch are independent values:
slot reuse changes object generation, provider restart changes lease, and
tree revocation removes child authority without minting either of the other
two. This refines `kernel/object::identity::CapabilityEpochs` and the IPC
transfer ticket's ObjectIdentity adapter.
*******************************************************************************)

Rights == {"read", "write", "transfer"}
Kinds == {"file", "job"}
Epochs == 1..2

Live == "live"
Absent == "absent"
Revoked == "revoked"

VARIABLES parentState, childState, parentRights, childRights,
          parentLease, childLease, parentRevoke, childRevoke, outcome,
          parentKind, childKind

vars == <<parentState, childState, parentRights, childRights,
          parentLease, childLease, parentRevoke, childRevoke, outcome,
          parentKind, childKind>>

Init ==
    /\ parentState = Live
    /\ childState = Absent
    /\ parentRights = {"read", "transfer"}
    /\ childRights = {}
    /\ parentLease = 1
    /\ childLease = 0
    /\ parentRevoke = 1
    /\ childRevoke = 0
    /\ outcome = "idle"
    /\ parentKind \in Kinds
    /\ childKind = "none"

Derive(kind, rights, lease, revoke) ==
    /\ parentState = Live
    /\ childState = Absent
    /\ rights \subseteq parentRights
    /\ kind = parentKind \* same object kind
    /\ lease = parentLease
    /\ revoke = parentRevoke
    /\ childState' = Live
    /\ childRights' = rights
    /\ childLease' = lease
    /\ childRevoke' = revoke
    /\ outcome' = "derived"
    /\ childKind' = kind
    /\ UNCHANGED <<parentState, parentRights, parentLease, parentRevoke, parentKind>>

DeriveAny ==
    \E kind \in Kinds, rights \in SUBSET Rights, lease \in Epochs, revoke \in Epochs:
        Derive(kind, rights, lease, revoke)

RejectBroadenOrEpochDrift(kind, rights, lease, revoke) ==
    /\ parentState = Live
    /\ childState = Absent
    /\ ~(rights \subseteq parentRights) \/ kind # parentKind \/ lease # parentLease \/ revoke # parentRevoke
    /\ outcome' = "rejected"
    /\ UNCHANGED <<parentState, childState, parentRights, childRights,
                    parentLease, childLease, parentRevoke, childRevoke, parentKind, childKind>>

RejectAny ==
    \E kind \in Kinds, rights \in SUBSET Rights, lease \in Epochs, revoke \in Epochs:
        RejectBroadenOrEpochDrift(kind, rights, lease, revoke)

RevokeChild ==
    /\ childState = Live
    /\ childState' = Revoked
    /\ outcome' = "child-revoked"
    /\ UNCHANGED <<parentState, parentRights, childRights, parentLease,
                    childLease, parentRevoke, childRevoke, parentKind, childKind>>

RevokeParent ==
    /\ parentState = Live
    /\ parentState' = Revoked
    /\ childState' = IF childState = Live THEN Revoked ELSE childState
    /\ outcome' = "parent-revoked"
    /\ UNCHANGED <<parentRights, childRights, parentLease, childLease,
                    parentRevoke, childRevoke, parentKind, childKind>>

RotateProviderLease ==
    /\ parentState = Live
    /\ parentLease < 2
    /\ parentLease' = parentLease + 1
    /\ childState' = IF childState = Live THEN Revoked ELSE childState
    /\ outcome' = "lease-rotated"
    /\ UNCHANGED <<parentState, parentRights, childRights, childLease,
                    parentRevoke, childRevoke, parentKind, childKind>>

UseChild ==
    /\ childState = Live
    /\ parentState = Live
    /\ childRights \subseteq parentRights
    /\ childKind = parentKind
    /\ childLease = parentLease
    /\ childRevoke = parentRevoke
    /\ outcome' = "used"
    /\ UNCHANGED <<parentState, childState, parentRights, childRights,
                    parentLease, childLease, parentRevoke, childRevoke, parentKind, childKind>>

RejectStale ==
    /\ childState = Revoked \/ parentState = Revoked
    /\ outcome' = "stale-rejected"
    /\ UNCHANGED <<parentState, childState, parentRights, childRights,
                    parentLease, childLease, parentRevoke, childRevoke, parentKind, childKind>>

Stutter ==
    /\ UNCHANGED vars

Next ==
    \/ DeriveAny
    \/ RejectAny
    \/ RevokeChild
    \/ RevokeParent
    \/ RotateProviderLease
    \/ UseChild
    \/ RejectStale
    \/ Stutter

TypeOK ==
    /\ parentState \in {Live, Revoked}
    /\ childState \in {Absent, Live, Revoked}
    /\ parentRights \subseteq Rights
    /\ childRights \subseteq Rights
    /\ parentKind \in Kinds
    /\ childKind \in Kinds \cup {"none"}
    /\ parentLease \in Epochs
    /\ childLease \in Epochs \cup {0}
    /\ parentRevoke \in Epochs
    /\ childRevoke \in Epochs \cup {0}
    /\ outcome \in {"idle", "derived", "rejected", "child-revoked", "parent-revoked",
                     "lease-rotated", "used", "stale-rejected"}

LiveChildIsAttenuatedAndExact ==
    childState = Live =>
        /\ parentState = Live
        /\ childRights \subseteq parentRights
        /\ childKind = parentKind
        /\ childLease = parentLease
        /\ childRevoke = parentRevoke

ParentRevokeLeavesNoLiveChild ==
    parentState = Revoked => childState # Live

StaleChildCannotUse ==
    outcome = "used" => childState = Live /\ parentState = Live

Spec == Init /\ [][Next]_vars
=============================================================================
