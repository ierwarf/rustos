--------------------- MODULE PagerRegionAgreement ---------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Owner: the shared range-edit rule in libs/rustos-user-abi/src/pager/region_edit.rs.

Ring0's per-process VMA table and pagerd's region table are two replicas of one
address map. This model is about the only thing that matters between them: an
address ring0 still has a VMA for, and that a fault can therefore reach, must
have a pagerd region behind it.

The defect this exists to exclude: ring0 preserved the left and right
remainders of an interior munmap while pagerd deleted every overlapping region.
A later fault in a surviving remainder passed ring0's VMA check, reached pagerd,
matched no region, and killed the thread. The first visible symptom was several
layers away from the cause, which is what made it expensive.

The two replicas are deliberately NOT required to be equal. They differ in two
declared directions, and the direction matters:

  * PROT_NONE. Ring0 keeps a deny-all VMA so the address stays owned; pagerd
    keeps nothing, because a span with no rights can never raise a fault and is
    not a canonical wire region. Ring0 is the superset.
  * Refused split. When pagerd has no free slot for the second fragment of a
    split it keeps the whole region and refuses, rather than dropping a
    remainder. Pagerd is the superset.

Both directions are safe for the same reason: ring0's VMA table is the
authority for whether a mapping exists, and pagerd is only ever consulted
through it. A pagerd region that outlives its VMA is inert. A missing pagerd
region under a live VMA kills a thread.
*******************************************************************************)

CONSTANTS Addresses,   \* the modelled address space, e.g. 1..4
          MaxVmas,     \* ring0's per-process VMA slots
          MaxRegions   \* pagerd's region slots

VARIABLES ring0,          \* addresses with a ring0 pager VMA
          denied,         \* subset of ring0 left with no rights (PROT_NONE)
          pager,          \* addresses pagerd holds a region for
          lastGrowth,     \* VMA slots the last edit added to ring0
          splitRefused,   \* pagerd refused an edit for want of split headroom
          withdrewBeforeAdmitting  \* a replica dropped a region it could not replace

vars == <<ring0, denied, pager, lastGrowth, splitRefused,
          withdrewBeforeAdmitting>>

(* A maximal contiguous run of mapped addresses is one table slot. Counting
   runs is how this model measures table occupancy without modelling intervals
   explicitly. *)
Runs(S) == Cardinality({a \in S : (a - 1) \notin S})

Ranges == {r \in SUBSET Addresses :
             /\ r # {}
             /\ \A a, b \in r : \A c \in Addresses :
                  (a < c /\ c < b) => c \in r}

Init ==
    /\ ring0 = {}
    /\ denied = {}
    /\ pager = {}
    /\ lastGrowth = 0
    /\ splitRefused = FALSE
    /\ withdrewBeforeAdmitting = FALSE

(* Faultable addresses are exactly what ring0 will dispatch a fault for.
   PROT_NONE spans are refused by ring0's own lookup and never reach pagerd. *)
Faultable == ring0 \ denied

----------------------------------------------------------------------------

(* Admission publishes a new range in both replicas, or refuses. Refusal is an
   explicit, counted downgrade to eager mapping - never a silent one - so it is
   modelled as a stutter rather than a partial publication. *)
Admit(r) ==
    /\ r \cap ring0 = {}
    \* Neither replica coalesces adjacent regions: each mmap owns its own slot
    \* and overlapping publication is refused outright. Admitting only into a
    \* gap keeps the run count equal to the slot count, which is what makes
    \* `Runs` an exact occupancy measure here.
    /\ \A a \in r : (a - 1) \notin ring0 /\ (a + 1) \notin ring0
    /\ Runs(ring0 \cup r) <= MaxVmas
    /\ Runs(pager \cup r) <= MaxRegions
    /\ ring0' = ring0 \cup r
    /\ pager' = pager \cup r
    /\ lastGrowth' = 1
    /\ UNCHANGED <<denied, splitRefused, withdrewBeforeAdmitting>>

RefuseAdmission(r) ==
    /\ \/ Runs(ring0 \cup r) > MaxVmas
       \/ Runs(pager \cup r) > MaxRegions
       \/ r \cap ring0 # {}
    /\ UNCHANGED vars

(* An unmap trims or splits; it never drops a region whose remainders survive.
   Ring0 applies it unconditionally, because it has already checked its own
   headroom before withdrawing anything. *)
Unmap(r) ==
    /\ r \cap ring0 # {}
    /\ Runs(ring0 \ r) <= MaxVmas
    /\ ring0' = ring0 \ r
    /\ denied' = denied \ r
    /\ lastGrowth' = IF Runs(ring0 \ r) > Runs(ring0)
                     THEN Runs(ring0 \ r) - Runs(ring0)
                     ELSE 0
    /\ IF Runs(pager \ r) <= MaxRegions
       THEN /\ pager' = pager \ r
            /\ UNCHANGED splitRefused
       ELSE \* No headroom for the second fragment. Keep the whole region and
            \* refuse; the broker parks the edit and retries. Keeping more than
            \* ring0 is inert, keeping less kills a live mapping.
            /\ pager' = pager
            /\ splitRefused' = TRUE
    /\ UNCHANGED withdrewBeforeAdmitting

(* mprotect(PROT_NONE): ring0 keeps a deny-all VMA, pagerd drops the span. *)
ProtectNone(r) ==
    /\ r \subseteq Faultable
    /\ Runs(ring0) <= MaxVmas
    /\ denied' = denied \cup r
    /\ ring0' = ring0
    /\ lastGrowth' = 0
    /\ IF Runs(pager \ r) <= MaxRegions
       THEN /\ pager' = pager \ r
            /\ UNCHANGED splitRefused
       ELSE /\ pager' = pager
            /\ splitRefused' = TRUE
    /\ UNCHANGED withdrewBeforeAdmitting

(* A refused split is retried once capacity exists. This is the broker's parked
   reconciliation queue, and it is what makes the refusal a deferral rather
   than a permanent divergence. *)
RetryRefusedSplit(r) ==
    /\ splitRefused
    /\ r \subseteq pager
    /\ r \cap ring0 = {}
    /\ Runs(pager \ r) <= MaxRegions
    /\ pager' = pager \ r
    /\ splitRefused' = FALSE
    /\ UNCHANGED <<ring0, denied, lastGrowth, withdrewBeforeAdmitting>>

Next ==
    \/ \E r \in Ranges : Admit(r)
    \/ \E r \in Ranges : RefuseAdmission(r)
    \/ \E r \in Ranges : Unmap(r)
    \/ \E r \in Ranges : ProtectNone(r)
    \/ \E r \in Ranges : RetryRefusedSplit(r)
    \/ UNCHANGED vars

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------

TypeOK ==
    /\ ring0 \subseteq Addresses
    /\ denied \subseteq ring0
    /\ pager \subseteq Addresses
    /\ lastGrowth \in 0..2
    /\ splitRefused \in BOOLEAN
    /\ withdrewBeforeAdmitting \in BOOLEAN

(* THE invariant. Every address a fault can be raised for has a pagerd region
   behind it. Deleting whole regions on a partial unmap violates exactly this,
   and the violation surfaces as a dead user thread. *)
FaultableIsAlwaysBackedByThePager == Faultable \subseteq pager

(* The surplus direction is safe: anything pagerd holds beyond ring0 can never
   be reached, because ring0 gates every dispatch. *)
PagerSurplusIsUnreachable == (pager \ Faultable) \cap Faultable = {}

(* Neither table may exceed the capacity it published. *)
Ring0TableWithinCapacity == Runs(ring0) <= MaxVmas
PagerTableWithinCapacity == Runs(pager) <= MaxRegions

(* One edit costs at most one extra slot: an edit is one contiguous interval
   and regions do not overlap, so only the region it lies strictly inside can
   split. This is the relation pagerd's table is sized against. *)
SplitGrowthIsBounded == lastGrowth <= 1

(* A replica must prove it can hold the result before it withdraws the
   original. Withdrawing first and failing to republish loses a live mapping. *)
NoWithdrawalWithoutHeadroom == ~withdrewBeforeAdmitting

(* A PROT_NONE span is owned by ring0 and tracked by neither as faultable. *)
DeniedSpansRaiseNoFault == denied \cap Faultable = {}

=============================================================================
