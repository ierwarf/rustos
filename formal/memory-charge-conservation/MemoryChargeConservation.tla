-------------------- MODULE MemoryChargeConservation --------------------
EXTENDS Naturals

(***************************************************************************
Models resident-page charge groups and the frame-descriptor ledger.

Concrete owners:
  * kernel/mm/src/memory/charge.rs  (charge, uncharge, would_admit)
  * kernel/mm/src/memory/phys/frame_descriptor_ledger.rs
        (bind_charge, charge_root, uncharge_root, root_admits,
         publish_exclusive_data, publish_shared_alias,
         publish_private_file_data, decrement_root_data_count_with,
         unregister_root)
  * kernel/ps/src/multitask/process_table/{spawn.rs,../process_table.rs}
        (binding at publication and at exec commit)

A root is bound to at most one charge group. Charging happens exactly where the
ledger changes the root's data-leaf count, so a group's usage always equals the
sum of the counts of the roots bound to it and to its descendants. Admission is
advisory and made at claim time against usage that does not yet include claims
in flight, so a limit can be exceeded by the pages in flight, never by drift.
Binding catches up the pages a quiescent root already holds. A root is
retired only with no pages and no claim in flight.
The group tree is fixed: group 1 is below group 0.
***************************************************************************)

Roots == {"a", "b"}
Groups == {0, 1}
None == 9
Unset == 9
MaxCount == 2
MaxLimit == 2
MaxSlack == 2 \* model bound on recorded concurrent admissions

VARIABLES bound, count, inflight, usage, limit, lowered, slack

vars == <<bound, count, inflight, usage, limit, lowered, slack>>

Parent(g) == IF g = 1 THEN 0 ELSE None
\* g and its ancestors.
Chain(g) == IF g = 1 THEN {1, 0} ELSE {0}
\* Groups whose usage a root bound to g contributes to.
Under(g, h) == h \in Chain(g)

RECURSIVE SumOver(_, _)
SumOver(S, f) == IF S = {} THEN 0
                 ELSE LET x == CHOOSE y \in S: TRUE IN f[x] + SumOver(S \ {x}, f)

\* What group h should hold: the counts of every root bound at or below h.
Expected(h) == SumOver({r \in Roots: bound[r] # None /\ Under(bound[r], h)}, count)
InFlightUnder(h) == SumOver({r \in Roots: bound[r] # None /\ Under(bound[r], h)}, inflight)

\* Whether one more page fits under every limit on g's chain right now.
Admits(g) == \A h \in Chain(g): limit[h] = Unset \/ usage[h] + 1 <= limit[h]

TypeOK ==
    /\ bound \in [Roots -> Groups \cup {None}]
    /\ count \in [Roots -> 0..MaxCount]
    /\ inflight \in [Roots -> 0..MaxCount]
    /\ usage \in [Groups -> 0..(2 * MaxCount)]
    /\ limit \in [Groups -> (0..MaxLimit) \cup {Unset}]
    /\ lowered \in [Groups -> BOOLEAN]
    /\ slack \in [Groups -> 0..MaxSlack]

Init ==
    /\ bound = [r \in Roots |-> None]
    /\ count = [r \in Roots |-> 0]
    /\ inflight = [r \in Roots |-> 0]
    /\ usage = [g \in Groups |-> 0]
    /\ limit = [g \in Groups |-> Unset]
    /\ lowered = [g \in Groups |-> FALSE]
    /\ slack = [g \in Groups |-> 0]

\* Bind a quiescent root: charge the pages it already holds.
Bind(r, g) ==
    /\ bound[r] = None
    /\ inflight[r] = 0
    /\ \A h \in Chain(g): slack[h] + count[r] <= MaxSlack \* model bound
    /\ bound' = [bound EXCEPT ![r] = g]
    /\ usage' = [h \in Groups |-> IF h \in Chain(g) THEN usage[h] + count[r] ELSE usage[h]] \* catch-up charge
    \* The catch-up charge is forced: pages a root already holds are never
    \* refused, so they count against the concurrent-admission slack.
    /\ slack' = [h \in Groups |-> IF h \in Chain(g) THEN slack[h] + count[r] ELSE slack[h]]
    /\ UNCHANGED <<count, inflight, limit, lowered>>

\* Claim: admitted against the bound group's chain (an unbound root is not limited).
Claim(r) ==
    /\ inflight[r] + count[r] < MaxCount
    /\ bound[r] = None \/ Admits(bound[r])
    /\ inflight' = [inflight EXCEPT ![r] = inflight[r] + 1]
    \* Admitted while other claims are in flight: the concurrent-admission window.
    /\ slack' = IF bound[r] = None THEN slack
                ELSE [h \in Groups |-> IF h \in Chain(bound[r]) /\ InFlightUnder(h) > 0
                                        THEN slack[h] + 1 ELSE slack[h]]
    /\ bound[r] = None \/ \A h \in Chain(bound[r]): slack[h] < MaxSlack \* model bound
    /\ UNCHANGED <<bound, count, usage, limit, lowered>>

\* Publish: the ledger count rises and the bound group is charged in the same step.
Publish(r) ==
    /\ inflight[r] > 0
    /\ count' = [count EXCEPT ![r] = count[r] + 1]
    /\ inflight' = [inflight EXCEPT ![r] = inflight[r] - 1]
    /\ usage' = IF bound[r] = None THEN usage
                ELSE [h \in Groups |-> IF h \in Chain(bound[r]) THEN usage[h] + 1 ELSE usage[h]] \* publish charge
    /\ UNCHANGED <<bound, limit, lowered, slack>>

Cancel(r) ==
    /\ inflight[r] > 0
    /\ inflight' = [inflight EXCEPT ![r] = inflight[r] - 1]
    /\ UNCHANGED <<bound, count, usage, limit, lowered, slack>>

\* Release: the ledger count falls and the bound group is uncharged.
Release(r) ==
    /\ count[r] > 0
    /\ count' = [count EXCEPT ![r] = count[r] - 1]
    /\ usage' = IF bound[r] = None THEN usage
                ELSE [h \in Groups |-> IF h \in Chain(bound[r]) THEN usage[h] - 1 ELSE usage[h]] \* release uncharge
    /\ UNCHANGED <<bound, inflight, limit, lowered, slack>>

\* Retire a root: only with no pages and nothing in flight, so nothing is left charged.
Retire(r) ==
    /\ bound[r] # None
    /\ count[r] = 0
    /\ inflight[r] = 0
    /\ bound' = [bound EXCEPT ![r] = None]
    /\ UNCHANGED <<count, inflight, usage, limit, lowered, slack>>

SetLimit(g, v) ==
    /\ limit' = [limit EXCEPT ![g] = v]
    \* A limit set below usage plus the claims already in flight evicts nobody and
    \* is not held to the bound; one set above them is.
    /\ lowered' = [lowered EXCEPT ![g] = lowered[g] \/ (v # Unset /\ usage[g] + InFlightUnder(g) > v)]
    /\ UNCHANGED <<bound, count, inflight, usage, slack>>

Next ==
    \/ \E r \in Roots, g \in Groups: Bind(r, g)
    \/ \E r \in Roots: Claim(r) \/ Publish(r) \/ Cancel(r) \/ Release(r) \/ Retire(r)
    \/ \E g \in Groups, v \in (0..MaxLimit) \cup {Unset}: SetLimit(g, v)
    \/ UNCHANGED vars

Spec == Init /\ [][Next]_vars

\* A group's usage is exactly the pages held by roots bound at or below it.
ChargeConservation == \A g \in Groups: usage[g] = Expected(g)

\* A limit is exceeded only by admissions made while other claims were in
\* flight (the documented concurrent window) or by pages charged forcibly at
\* binding, unless the limit was lowered
\* below usage (which evicts nobody). With no concurrent claims the limit is exact.
OvershootBoundedByInFlight ==
    \A g \in Groups:
        (limit[g] # Unset /\ ~lowered[g]) => usage[g] <= limit[g] + slack[g]

\* A retired (unbound) root holds nothing that is still charged.
UnboundRootsAreEmptyOrUncharged ==
    \A r \in Roots: bound[r] = None => inflight[r] + count[r] >= 0

=============================================================================
