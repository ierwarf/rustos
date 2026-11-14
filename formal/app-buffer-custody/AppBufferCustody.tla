----------------------------- MODULE AppBufferCustody -----------------------------
EXTENDS Naturals

(***************************************************************************
Bounded projection of the DMA-BUF aperture custody in
app_buffer_broker_ops.rs, device_buffer.rs and dvm_display/app_buffer.rs: two
disjoint aperture ranges sharing ONE broker pending slot, two devmgrd service
epochs, two incarnations of the requesting process, a provider that may
export/answer/ack late, a client that holds committed descriptors, and a
per-request deadline. No caller chooses an offset.
Properties: a descriptor is committed only after a timely provider response
under the exact service epoch and process/MM incarnation; every failure,
timeout or restart keeps the range quarantined until the provider's revoke
acknowledgement and a scrub; a range is reallocated only when clean; an
unpublished range (never exported) may be retired locally; one range's custody
never releases or commits another's.
Not modelled: aperture identity verification, per-page mapping and TLB
acknowledgement (device_buffer native tests), the pool's byte capacity, the
byte-level provider protocol, and IPC delivery.
***************************************************************************)
Ranges == 1..2
Pending == {"published", "answered", "revoking"}
VARIABLES st, exported, dirty, epoch, allocEpoch, procInc, allocInc, expired,
          fd, lateCommit
vars == <<st, exported, dirty, epoch, allocEpoch, procInc, allocInc, expired,
          fd, lateCommit>>

NoPending == \A q \in Ranges: st[q] \notin Pending

Init ==
    /\ st = [r \in Ranges |-> "free"] /\ exported = [r \in Ranges |-> FALSE]
    /\ dirty = [r \in Ranges |-> FALSE]
    /\ epoch = 1 /\ allocEpoch = [r \in Ranges |-> 1]
    /\ procInc = 1 /\ allocInc = [r \in Ranges |-> 1]
    /\ expired = [r \in Ranges |-> FALSE] /\ fd = [r \in Ranges |-> FALSE]
    /\ lateCommit = FALSE

\* Devmgrd-only allocation of a clean range for the exact current incarnation.
Alloc(r) ==
    /\ st[r] = "free" /\ ~dirty[r]
    /\ st' = [st EXCEPT ![r] = "allocated"]
    /\ allocEpoch' = [allocEpoch EXCEPT ![r] = epoch]
    /\ allocInc' = [allocInc EXCEPT ![r] = procInc]
    /\ expired' = [expired EXCEPT ![r] = FALSE]
    /\ UNCHANGED <<exported, dirty, epoch, procInc, fd, lateCommit>>

\* The export request is published to the provider; the provider may now hold
\* it. Only one request is pending in the broker at a time.
Publish(r) ==
    /\ st[r] = "allocated" /\ epoch = allocEpoch[r] /\ NoPending
    /\ st' = [st EXCEPT ![r] = "published"]
    /\ exported' = [exported EXCEPT ![r] = TRUE]
    /\ UNCHANGED <<dirty, epoch, allocEpoch, procInc, allocInc, expired, fd,
                    lateCommit>>

\* Publication refused or lost its lease race: nothing was ever exported.
AbandonUnpublished(r) ==
    /\ st[r] = "allocated"
    /\ ~exported[r]
    /\ st' = [st EXCEPT ![r] = "retired"]
    /\ UNCHANGED <<exported, dirty, epoch, allocEpoch, procInc, allocInc,
                    expired, fd, lateCommit>>

ProviderAnswer(r) ==
    /\ st[r] = "published"
    /\ st' = [st EXCEPT ![r] = "answered"]
    /\ UNCHANGED <<exported, dirty, epoch, allocEpoch, procInc, allocInc,
                    expired, fd, lateCommit>>

\* Commit only for the exact epoch and incarnation, before the deadline.
Commit(r) ==
    /\ st[r] = "answered"
    /\ ~expired[r]
    /\ epoch = allocEpoch[r]
    /\ procInc = allocInc[r]
    /\ st' = [st EXCEPT ![r] = "active"]
    /\ fd' = [fd EXCEPT ![r] = TRUE]
    /\ dirty' = [dirty EXCEPT ![r] = TRUE]
    /\ lateCommit' = (lateCommit \/ expired[r] \/ epoch /= allocEpoch[r]
                       \/ procInc /= allocInc[r])
    /\ UNCHANGED <<exported, epoch, allocEpoch, procInc, allocInc, expired>>

\* Pending custody that cannot commit, or an active export whose descriptor was
\* closed or whose provider epoch ended, begins a provider revoke; the range
\* stays quarantined and the provider export is still live.
BeginRevoke(r) ==
    /\ \/ st[r] \in {"published", "answered"}
       \/ st[r] = "active" /\ NoPending /\ (~fd[r] \/ epoch /= allocEpoch[r])
    /\ st' = [st EXCEPT ![r] = "revoking"]
    /\ fd' = [fd EXCEPT ![r] = FALSE]
    /\ UNCHANGED <<exported, dirty, epoch, allocEpoch, procInc, allocInc,
                    expired, lateCommit>>

\* Only the provider's acknowledgement ends the export.
ProviderAck(r) ==
    /\ st[r] = "revoking"
    /\ st' = [st EXCEPT ![r] = "retired"]
    /\ exported' = [exported EXCEPT ![r] = FALSE]
    /\ UNCHANGED <<dirty, epoch, allocEpoch, procInc, allocInc, expired, fd,
                    lateCommit>>

\* Zero the range before it can be allocated again.
Scrub(r) ==
    /\ st[r] = "retired" /\ ~exported[r]
    /\ st' = [st EXCEPT ![r] = "free"]
    /\ dirty' = [dirty EXCEPT ![r] = FALSE]
    /\ UNCHANGED <<exported, epoch, allocEpoch, procInc, allocInc, expired, fd,
                    lateCommit>>

ClientCloses(r) ==
    /\ st[r] = "active" /\ fd[r]
    /\ fd' = [fd EXCEPT ![r] = FALSE]
    /\ UNCHANGED <<st, exported, dirty, epoch, allocEpoch, procInc, allocInc,
                    expired, lateCommit>>

Expire(r) ==
    /\ ~expired[r] /\ st[r] \in {"allocated", "published", "answered"}
    /\ expired' = [expired EXCEPT ![r] = TRUE]
    /\ UNCHANGED <<st, exported, dirty, epoch, allocEpoch, procInc, allocInc,
                    fd, lateCommit>>

ProviderRestart ==
    /\ epoch < 2
    /\ epoch' = epoch + 1
    /\ UNCHANGED <<st, exported, dirty, allocEpoch, procInc, allocInc, expired,
                    fd, lateCommit>>

ProcessReincarnate ==
    /\ procInc < 2
    /\ procInc' = procInc + 1
    /\ UNCHANGED <<st, exported, dirty, epoch, allocEpoch, allocInc, expired,
                    fd, lateCommit>>

Stutter == UNCHANGED vars

Next ==
    \/ \E r \in Ranges:
        Alloc(r) \/ Publish(r) \/ AbandonUnpublished(r) \/ ProviderAnswer(r)
        \/ Commit(r) \/ BeginRevoke(r) \/ ProviderAck(r) \/ Scrub(r)
        \/ ClientCloses(r) \/ Expire(r)
    \/ ProviderRestart \/ ProcessReincarnate
    \/ Stutter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ st \in [Ranges -> {"free", "allocated", "published", "answered", "active",
                          "revoking", "retired"}]
    /\ exported \in [Ranges -> BOOLEAN] /\ dirty \in [Ranges -> BOOLEAN]
    /\ fd \in [Ranges -> BOOLEAN] /\ expired \in [Ranges -> BOOLEAN]
    /\ epoch \in 1..2 /\ allocEpoch \in [Ranges -> 1..2]
    /\ procInc \in 1..2 /\ allocInc \in [Ranges -> 1..2]
    /\ lateCommit \in BOOLEAN

\* A free or allocatable range never holds old bytes.
NoReuseWhileDirty == \A r \in Ranges: st[r] \in {"free", "allocated"} => ~dirty[r]
\* A free range is never still exported to the provider.
NoReuseWhileExported == \A r \in Ranges: st[r] = "free" => ~exported[r]
\* Retirement requires the provider acknowledgement (or no export ever existed).
RetiredNeedsAck == \A r \in Ranges: st[r] = "retired" => ~exported[r]
\* A descriptor exists only for an active, timely, exact-incarnation commit.
DescriptorOnlyWhenActive == \A r \in Ranges: fd[r] => st[r] = "active"
NoLateOrStaleCommit == ~lateCommit
\* The broker has one pending request slot, shared by every range.
SinglePendingRequest ==
    \A q, r \in Ranges: (st[q] \in Pending /\ st[r] \in Pending) => q = r
===============================================================================
