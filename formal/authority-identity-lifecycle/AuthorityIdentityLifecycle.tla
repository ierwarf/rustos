--------------------- MODULE AuthorityIdentityLifecycle ---------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Bounded abstraction of kernel-minted task IDs, process-slot generations,
open-description tokens, prepare handles, and exec tickets.

An authority identity is never wrapped back into the live namespace. Revoke
moves it to a retired set permanently; exhaustion rejects future allocation or
retires the exhausted slot instead of aliasing a stale bearer.
*******************************************************************************)

MaxIdentity == 3
Identities == 1..MaxIdentity

VARIABLES nextIdentity, active, retired, outcome

vars == <<nextIdentity, active, retired, outcome>>

Init ==
    /\ nextIdentity = 1
    /\ active = {}
    /\ retired = {}
    /\ outcome = "idle"

Mint ==
    /\ nextIdentity \in Identities
    /\ nextIdentity \notin active \cup retired
    /\ active' = active \cup {nextIdentity}
    /\ nextIdentity' = nextIdentity + 1
    /\ outcome' = "minted"
    /\ UNCHANGED retired

Revoke(id) ==
    /\ id \in active
    /\ active' = active \ {id}
    /\ retired' = retired \cup {id}
    /\ outcome' = "revoked"
    /\ UNCHANGED nextIdentity

UseLive(id) ==
    /\ id \in active
    /\ outcome' = "used"
    /\ UNCHANGED <<nextIdentity, active, retired>>

RejectStale(id) ==
    /\ id \in retired
    /\ outcome' = "stale-rejected"
    /\ UNCHANGED <<nextIdentity, active, retired>>

Exhaust ==
    /\ nextIdentity > MaxIdentity
    /\ outcome' = "exhausted"
    /\ UNCHANGED <<nextIdentity, active, retired>>

Next ==
    \/ Mint
    \/ \E id \in Identities: Revoke(id)
    \/ \E id \in Identities: UseLive(id)
    \/ \E id \in Identities: RejectStale(id)
    \/ Exhaust

TypeOK ==
    /\ nextIdentity \in 1..(MaxIdentity + 1)
    /\ active \subseteq Identities
    /\ retired \subseteq Identities
    /\ outcome \in
        {"idle", "minted", "revoked", "used", "stale-rejected", "exhausted"}

NoAuthorityAlias == active \cap retired = {}

RetiredIdentityNeverBecomesLive ==
    \A id \in retired: id \notin active

AllocationNeverWraps == nextIdentity >= 1

ExhaustionNeverMints ==
    nextIdentity > MaxIdentity => nextIdentity \notin active

Spec == Init /\ [][Next]_vars
=============================================================================
