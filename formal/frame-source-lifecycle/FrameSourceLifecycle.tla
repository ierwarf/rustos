------------------------ MODULE FrameSourceLifecycle ------------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
One reusable slot, two token generations, two service lease epochs, two roots.
Source ownership and target-leaf ownership are independent. A source may leave
its slot while old mapped leaves still hold its physical frame. Supply is
serialized with registry revoke, and last-pin Drop queues, never reclaims.
This bounded safety projection does not model file I/O, x86 PTE publication,
allocator success, cache policy or the weak-memory intrusive queue algorithm.
***************************************************************************)
Gens == 1..2
Roots == 1..2
VARIABLES state, gen, epoch, registered, revoked, registryBusy,
          sourceEpoch, binding, contents, pins, cache, charged, valid, pending,
          freed, privateRoots, rights, acceptedToken, acceptedBinding,
          acceptedOwner, droppedPhysically
vars == <<state, gen, epoch, registered, revoked, registryBusy, sourceEpoch,
          binding, contents, pins, cache, charged, valid, pending, freed,
          privateRoots, rights, acceptedToken, acceptedBinding, acceptedOwner,
          droppedPhysically>>

Init ==
    /\ state = "free" /\ gen = 0 /\ epoch = 1 /\ registered = TRUE
    /\ revoked = {} /\ registryBusy = FALSE
    /\ sourceEpoch = [g \in Gens |-> 0]
    /\ binding = [g \in Gens |-> 0] /\ contents = [g \in Gens |-> 0]
    /\ pins = [g \in Gens |-> 0]
    /\ cache = [g \in Gens |-> FALSE] /\ charged = cache
    /\ valid = [g \in Gens |-> {}] /\ pending = valid
    /\ freed = [g \in Gens |-> TRUE] /\ privateRoots = {}
    /\ rights = [g \in Gens |-> <<FALSE, FALSE>>]
    /\ acceptedToken = TRUE /\ acceptedBinding = TRUE /\ acceptedOwner = TRUE
    /\ droppedPhysically = FALSE

BeginSupply(authorized, key, writable, executable) ==
    /\ authorized /\ registered /\ ~registryBusy /\ state = "free" /\ gen < 2
    /\ key \in Gens /\ ~(writable /\ executable)
    /\ gen' = gen + 1 /\ state' = "publishing" /\ registryBusy' = TRUE
    /\ sourceEpoch' = [sourceEpoch EXCEPT ![gen + 1] = epoch]
    /\ binding' = [binding EXCEPT ![gen + 1] = key]
    /\ contents' = [contents EXCEPT ![gen + 1] = key]
    /\ cache' = [cache EXCEPT ![gen + 1] = TRUE]
    /\ charged' = [charged EXCEPT ![gen + 1] = TRUE]
    /\ freed' = [freed EXCEPT ![gen + 1] = FALSE]
    /\ rights' = [rights EXCEPT ![gen + 1] = <<writable, executable>>]
    /\ UNCHANGED <<epoch, registered, revoked, pins, valid, pending,
                    privateRoots, acceptedToken, acceptedBinding, acceptedOwner,
                    droppedPhysically>>

Publish ==
    /\ state = "publishing" /\ registryBusy /\ registered
    /\ sourceEpoch[gen] = epoch
    /\ state' = "live" /\ registryBusy' = FALSE
    /\ UNCHANGED <<gen, epoch, registered, revoked, sourceEpoch, binding,
                    contents, pins, cache, charged, valid, pending, freed,
                    privateRoots, rights, acceptedToken, acceptedBinding,
                    acceptedOwner, droppedPhysically>>

RevokeService ==
    /\ registered /\ ~registryBusy
    /\ registered' = FALSE /\ revoked' = revoked \cup {epoch}
    /\ UNCHANGED <<state, gen, epoch, registryBusy, sourceEpoch, binding,
                    contents, pins, cache, charged, valid, pending, freed,
                    privateRoots, rights, acceptedToken, acceptedBinding,
                    acceptedOwner, droppedPhysically>>

RegisterReplacement ==
    /\ ~registered /\ epoch < 2 /\ ~registryBusy
    /\ registered' = TRUE /\ epoch' = epoch + 1
    /\ UNCHANGED <<state, gen, revoked, registryBusy, sourceEpoch, binding,
                    contents, pins, cache, charged, valid, pending, freed,
                    privateRoots, rights, acceptedToken, acceptedBinding,
                    acceptedOwner, droppedPhysically>>

Pin(token, key, ownerEpoch) ==
    /\ state = "live" /\ registered /\ pins[gen] < 2
    /\ token = gen
    /\ key = binding[gen]
    /\ ownerEpoch = epoch /\ ownerEpoch = sourceEpoch[gen]
    /\ pins' = [pins EXCEPT ![gen] = @ + 1]
    /\ acceptedToken' = (token = gen)
    /\ acceptedBinding' = (key = binding[gen])
    /\ acceptedOwner' = (ownerEpoch = sourceEpoch[gen])
    /\ UNCHANGED <<state, gen, epoch, registered, revoked, registryBusy,
                    sourceEpoch, binding, contents, cache, charged, valid,
                    pending, freed, privateRoots, rights, droppedPhysically>>

CloseSource(ownerEpoch) ==
    /\ state = "live" /\ ownerEpoch = sourceEpoch[gen]
    /\ \/ /\ registered /\ ownerEpoch = epoch
       \/ ownerEpoch \in revoked
    /\ state' = IF pins[gen] = 0 THEN "queued" ELSE "closing"
    /\ UNCHANGED <<gen, epoch, registered, revoked, registryBusy, sourceEpoch,
                    binding, contents, pins, cache, charged, valid, pending,
                    freed, privateRoots, rights, acceptedToken, acceptedBinding,
                    acceptedOwner, droppedPhysically>>

DropPin ==
    /\ state \in {"live", "closing"} /\ pins[gen] > 0
    /\ pins' = [pins EXCEPT ![gen] = @ - 1]
    /\ state' = IF state = "closing" /\ pins[gen] = 1 THEN "queued" ELSE state
    /\ droppedPhysically' = FALSE
    /\ UNCHANGED <<gen, epoch, registered, revoked, registryBusy, sourceEpoch,
                    binding, contents, cache, charged, valid, pending, freed,
                    privateRoots, rights, acceptedToken, acceptedBinding, acceptedOwner>>

Install(root) ==
    /\ state \in {"live", "closing"} /\ pins[gen] > 0
    /\ root \in Roots /\ root \notin privateRoots
    /\ \A g \in Gens: root \notin valid[g] \cup pending[g]
    /\ valid' = [valid EXCEPT ![gen] = @ \cup {root}]
    /\ UNCHANGED <<state, gen, epoch, registered, revoked, registryBusy,
                    sourceEpoch, binding, contents, pins, cache, charged,
                    pending, freed, privateRoots, rights, acceptedToken,
                    acceptedBinding, acceptedOwner, droppedPhysically>>

Invalidate(g, root, cow) ==
    /\ g \in Gens /\ root \in valid[g]
    /\ valid' = [valid EXCEPT ![g] = @ \ {root}]
    /\ pending' = [pending EXCEPT ![g] = @ \cup {root}]
    /\ privateRoots' = IF cow THEN privateRoots \cup {root} ELSE privateRoots
    /\ UNCHANGED <<state, gen, epoch, registered, revoked, registryBusy,
                    sourceEpoch, binding, contents, pins, cache, charged, freed,
                    rights, acceptedToken, acceptedBinding, acceptedOwner,
                    droppedPhysically>>

TlbAck(g, root) ==
    /\ g \in Gens /\ root \in pending[g]
    /\ pending' = [pending EXCEPT ![g] = @ \ {root}]
    /\ freed' = [freed EXCEPT ![g] = ~cache[g] /\ valid[g] = {} /\ pending[g] = {root}]
    /\ UNCHANGED <<state, gen, epoch, registered, revoked, registryBusy,
                    sourceEpoch, binding, contents, pins, cache, charged, valid,
                    privateRoots, rights, acceptedToken, acceptedBinding,
                    acceptedOwner, droppedPhysically>>

ColdReclaim(normalContext) ==
    /\ state = "queued" /\ pins[gen] = 0 /\ normalContext
    /\ cache' = [cache EXCEPT ![gen] = FALSE]
    /\ charged' = [charged EXCEPT ![gen] = FALSE]
    /\ freed' = [freed EXCEPT ![gen] = valid[gen] = {} /\ pending[gen] = {}]
    /\ state' = "free"
    /\ UNCHANGED <<gen, epoch, registered, revoked, registryBusy, sourceEpoch,
                    binding, contents, pins, valid, pending, privateRoots,
                    rights, acceptedToken, acceptedBinding, acceptedOwner,
                    droppedPhysically>>

ReleasePrivate(root) ==
    /\ root \in privateRoots /\ privateRoots' = privateRoots \ {root}
    /\ UNCHANGED <<state, gen, epoch, registered, revoked, registryBusy,
                    sourceEpoch, binding, contents, pins, cache, charged, valid,
                    pending, freed, rights, acceptedToken, acceptedBinding,
                    acceptedOwner, droppedPhysically>>

Next ==
    \/ \E authorized \in BOOLEAN, key \in Gens, w \in BOOLEAN, x \in BOOLEAN: BeginSupply(authorized, key, w, x)
    \/ Publish \/ RevokeService \/ RegisterReplacement
    \/ \E token \in Gens, key \in Gens, e \in Gens: Pin(token, key, e)
    \/ \E e \in Gens: CloseSource(e)
    \/ DropPin \/ \E r \in Roots: Install(r)
    \/ \E g \in Gens, r \in Roots, cow \in BOOLEAN: Invalidate(g, r, cow)
    \/ \E g \in Gens, r \in Roots: TlbAck(g, r)
    \/ \E normal \in BOOLEAN: ColdReclaim(normal)
    \/ \E r \in Roots: ReleasePrivate(r)
    \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ state \in {"free", "publishing", "live", "closing", "queued"}
    /\ gen \in 0..2 /\ epoch \in Gens /\ registered \in BOOLEAN
    /\ revoked \subseteq Gens /\ registryBusy \in BOOLEAN
    /\ sourceEpoch \in [Gens -> 0..2] /\ binding \in [Gens -> 0..2]
    /\ contents \in [Gens -> 0..2] /\ pins \in [Gens -> 0..2]
    /\ cache \in [Gens -> BOOLEAN] /\ charged \in [Gens -> BOOLEAN]
    /\ valid \in [Gens -> SUBSET Roots] /\ pending \in [Gens -> SUBSET Roots]
    /\ freed \in [Gens -> BOOLEAN] /\ privateRoots \subseteq Roots
    /\ rights \in [Gens -> (BOOLEAN \X BOOLEAN)]

NoEarlyPhysicalReuse == \A g \in Gens:
    freed[g] => /\ ~cache[g] /\ pins[g] = 0 /\ valid[g] = {} /\ pending[g] = {}
SourceChargeFollowsCustody == \A g \in Gens: charged[g] = cache[g]
QueuedPinsAreDrained == state = "queued" => pins[gen] = 0
SlotReuseFollowsCold == (gen > 0 /\ state = "free") => ~cache[gen] /\ ~charged[gen] /\ pins[gen] = 0
NoOldPinAcrossReuse == \A g \in Gens: pins[g] > 0 => gen = g /\ state \in {"live", "closing"}
PublisherSerializedWithRevoke == state = "publishing" => registryBusy /\ registered /\ sourceEpoch[gen] = epoch
ExactTokenAndObject == acceptedToken /\ acceptedBinding /\ acceptedOwner
SourceBytesStayImmutable == \A g \in Gens: contents[g] = binding[g]
WxSafe == \A g \in Gens: ~(rights[g][1] /\ rights[g][2])
DropNeverReclaims == ~droppedPhysically
=============================================================================
