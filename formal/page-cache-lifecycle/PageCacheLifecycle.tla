------------------------- MODULE PageCacheLifecycle -------------------------
EXTENDS Naturals

(*******************************************************************************
Owner: pagerd file-page policy.
Linearization points: one exact load-token publication, dirty/writeback state
transition, and exact TLB acknowledgement before reclaimed frame release.
*******************************************************************************)

CONSTANTS Absent, Loading, Clean, Dirty, Writeback, Reclaiming, Revoked

VARIABLES state, keyGeneration, loadOwners, pageGeneration, dirty,
          writebackToken, tlbGeneration, tlbAck, freed, cowSourceClean,
          mappings, backingRevoked, revokeToken
vars == <<state, keyGeneration, loadOwners, pageGeneration, dirty,
          writebackToken, tlbGeneration, tlbAck, freed, cowSourceClean,
          mappings, backingRevoked, revokeToken>>

Init ==
    /\ state = Absent
    /\ keyGeneration = 0
    /\ loadOwners = 0
    /\ pageGeneration = 0
    /\ dirty = FALSE
    /\ writebackToken = 0
    /\ tlbGeneration = 0
    /\ tlbAck = 0
    /\ freed = FALSE
    /\ cowSourceClean = TRUE
    /\ mappings = 0
    /\ backingRevoked = FALSE
    /\ revokeToken = 0

BeginLoad(g) ==
    /\ state = Absent
    /\ ~backingRevoked
    /\ g > 0
    /\ state' = Loading
    /\ keyGeneration' = g
    /\ loadOwners' = 1
    /\ freed' = FALSE
    /\ mappings' = 0
    /\ revokeToken' = 0
    /\ UNCHANGED <<pageGeneration, dirty, writebackToken, tlbGeneration,
                    tlbAck, cowSourceClean, backingRevoked>>

Coalesce ==
    /\ state = Loading
    /\ UNCHANGED vars

PublishClean ==
    /\ state = Loading
    /\ loadOwners = 1
    /\ state' = Clean
    /\ pageGeneration' = 1
    /\ dirty' = FALSE
    /\ loadOwners' = 0
    /\ UNCHANGED <<keyGeneration, writebackToken, tlbGeneration, tlbAck,
                    freed, cowSourceClean, mappings, backingRevoked,
                    revokeToken>>

MapPage ==
    /\ state \in {Clean, Dirty, Writeback}
    /\ mappings = 0
    /\ mappings' = 1
    /\ UNCHANGED <<state, keyGeneration, loadOwners, pageGeneration, dirty,
                    writebackToken, tlbGeneration, tlbAck, freed,
                    cowSourceClean, backingRevoked, revokeToken>>

UnmapPage ==
    /\ state \in {Clean, Dirty, Writeback, Revoked}
    /\ mappings = 1
    /\ mappings' = 0
    /\ UNCHANGED <<state, keyGeneration, loadOwners, pageGeneration, dirty,
                    writebackToken, tlbGeneration, tlbAck, freed,
                    cowSourceClean, backingRevoked, revokeToken>>

PrivateCow ==
    /\ state = Clean
    /\ ~dirty
    /\ cowSourceClean' = TRUE
    /\ UNCHANGED <<state, keyGeneration, loadOwners, pageGeneration, dirty,
                    writebackToken, tlbGeneration, tlbAck, freed, mappings,
                    backingRevoked, revokeToken>>

MarkDirty ==
    /\ state = Clean
    /\ state' = Dirty
    /\ dirty' = TRUE
    /\ UNCHANGED <<keyGeneration, loadOwners, pageGeneration, writebackToken,
                    tlbGeneration, tlbAck, freed, cowSourceClean, mappings,
                    backingRevoked, revokeToken>>

BeginWriteback ==
    /\ state = Dirty
    /\ dirty
    /\ state' = Writeback
    /\ writebackToken' = 1
    /\ UNCHANGED <<keyGeneration, loadOwners, pageGeneration, dirty,
                    tlbGeneration, tlbAck, freed, cowSourceClean, mappings,
                    backingRevoked, revokeToken>>

CompleteWriteback ==
    /\ state = Writeback
    /\ writebackToken = 1
    /\ state' = Clean
    /\ dirty' = FALSE
    /\ writebackToken' = 0
    /\ UNCHANGED <<keyGeneration, loadOwners, pageGeneration, tlbGeneration,
                    tlbAck, freed, cowSourceClean, mappings, backingRevoked,
                    revokeToken>>

BeginReclaim ==
    /\ state = Clean
    /\ ~dirty
    /\ mappings = 0
    /\ state' = Reclaiming
    /\ tlbGeneration' = 1
    /\ tlbAck' = 0
    /\ UNCHANGED <<keyGeneration, loadOwners, pageGeneration, dirty,
                    writebackToken, freed, cowSourceClean, mappings,
                    backingRevoked, revokeToken>>

AckTlb ==
    /\ state \in {Reclaiming, Revoked}
    /\ tlbGeneration > 0
    /\ tlbAck' = tlbGeneration
    /\ UNCHANGED <<state, keyGeneration, loadOwners, pageGeneration, dirty,
                    writebackToken, tlbGeneration, freed, cowSourceClean,
                    mappings, backingRevoked, revokeToken>>

CompleteReclaim ==
    /\ state = Reclaiming
    /\ tlbGeneration > 0
    /\ tlbAck = tlbGeneration
    /\ state' = Absent
    /\ freed' = TRUE
    /\ UNCHANGED <<keyGeneration, loadOwners, pageGeneration, dirty,
                    writebackToken, tlbGeneration, tlbAck, cowSourceClean,
                    mappings, backingRevoked, revokeToken>>

RevokeLoading ==
    /\ state = Loading
    /\ state' = Absent
    /\ backingRevoked' = TRUE
    /\ revokeToken' = 1
    /\ loadOwners' = 0
    /\ UNCHANGED <<keyGeneration, pageGeneration, dirty, writebackToken,
                    tlbGeneration, tlbAck, freed, cowSourceClean, mappings>>

RevokePage ==
    /\ state \in {Clean, Dirty, Writeback, Reclaiming}
    /\ state' = Revoked
    /\ backingRevoked' = TRUE
    /\ revokeToken' = 1
    /\ loadOwners' = 0
    /\ UNCHANGED <<keyGeneration, pageGeneration, dirty, writebackToken,
                    tlbGeneration, tlbAck, freed, cowSourceClean, mappings>>

BeginRevokedReclaim ==
    /\ state = Revoked
    /\ pageGeneration > 0
    /\ ~dirty
    /\ mappings = 0
    /\ revokeToken = 1
    /\ tlbGeneration' = 1
    /\ tlbAck' = 0
    /\ UNCHANGED <<state, keyGeneration, loadOwners, pageGeneration, dirty,
                    writebackToken, freed, cowSourceClean, mappings,
                    backingRevoked, revokeToken>>

CompleteRevokedReclaim ==
    /\ state = Revoked
    /\ pageGeneration > 0
    /\ ~dirty
    /\ revokeToken = 1
    /\ tlbGeneration > 0
    /\ tlbAck = tlbGeneration
    /\ state' = Absent
    /\ freed' = TRUE
    /\ UNCHANGED <<keyGeneration, loadOwners, pageGeneration, dirty,
                    writebackToken, tlbGeneration, tlbAck, cowSourceClean,
                    mappings, backingRevoked, revokeToken>>

ReauthorizeDirty ==
    /\ state = Revoked
    /\ dirty
    /\ revokeToken = 1
    /\ keyGeneration = 1
    /\ state' = Dirty
    /\ keyGeneration' = 2
    /\ backingRevoked' = FALSE
    /\ revokeToken' = 0
    /\ UNCHANGED <<loadOwners, pageGeneration, dirty, writebackToken,
                    tlbGeneration, tlbAck, freed, cowSourceClean, mappings>>

DenyRevokedFault ==
    /\ state = Absent
    /\ backingRevoked
    /\ UNCHANGED vars

QuarantineRevokedDirty ==
    /\ state = Revoked
    /\ dirty
    /\ UNCHANGED vars

Next ==
    \/ \E g \in 1..2: BeginLoad(g)
    \/ Coalesce
    \/ PublishClean
    \/ MapPage
    \/ UnmapPage
    \/ PrivateCow
    \/ MarkDirty
    \/ BeginWriteback
    \/ CompleteWriteback
    \/ BeginReclaim
    \/ AckTlb
    \/ CompleteReclaim
    \/ RevokeLoading
    \/ RevokePage
    \/ BeginRevokedReclaim
    \/ CompleteRevokedReclaim
    \/ ReauthorizeDirty
    \/ DenyRevokedFault
    \/ QuarantineRevokedDirty

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ state \in {Absent, Loading, Clean, Dirty, Writeback, Reclaiming, Revoked}
    /\ keyGeneration \in 0..2
    /\ loadOwners \in 0..2
    /\ pageGeneration \in 0..1
    /\ writebackToken \in 0..1
    /\ tlbGeneration \in 0..1
    /\ tlbAck \in 0..1
    /\ dirty \in BOOLEAN
    /\ freed \in BOOLEAN
    /\ cowSourceClean \in BOOLEAN
    /\ mappings \in 0..1
    /\ backingRevoked \in BOOLEAN
    /\ revokeToken \in 0..1

OneLoadOwner == loadOwners <= 1
LiveEntryHasBackingGeneration == state = Absent \/ keyGeneration > 0
PrivateCowPreservesSharedClean == cowSourceClean
ReclaimStartsOnlyFromClean == state = Reclaiming => ~dirty /\ writebackToken = 0
FreedAfterExactTlbAck == freed => tlbGeneration > 0 /\ tlbAck = tlbGeneration
RevokedDirtyIsQuarantined == state = Revoked /\ dirty => ~freed
RevokedFrameWaitsForExactTlbAck ==
    state = Revoked /\ pageGeneration > 0 /\ (tlbGeneration = 0 \/ tlbAck # tlbGeneration)
        => ~freed
RevokedBackingCannotReload == backingRevoked => state # Loading

=============================================================================
