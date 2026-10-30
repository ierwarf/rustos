-------------------- MODULE JobLimits --------------------
EXTENDS Naturals

(***************************************************************************
Models counted limits on the job tree and admission against them.

Concrete owners:
  * kernel/ps/src/multitask/process_table/jobs.rs
        (JobLimits, own_limit, effective_limit, admits, job_set_limits)
  * kernel/ps/src/multitask/process_table/spawn.rs (publish_inner admission)
  * kernel/ps/src/multitask/process_table.rs (attach_task admission)

A job's limit is its own value, or unset to inherit. Admission of one more
process asks every job on the chain from the target to the root whether its
subtree count plus one still fits that job's own limit; one refusal refuses.
A limit lowered below current usage evicts nobody: it only stops admission.
The tree is fixed: 0 is the root, 1 is below 0, 2 is below 1.
***************************************************************************)

Jobs == 0..2
Unset == 9
MaxLimit == 2
MaxCount == 3

VARIABLES limit, count, overAdmitted

vars == <<limit, count, overAdmitted>>

Parent(j) == IF j = 0 THEN 0 ELSE j - 1

\* j and its ancestors, root last.
Chain(j) == {k \in Jobs: k <= j}

RECURSIVE SumCount(_)
SumCount(S) == IF S = {} THEN 0
               ELSE LET x == CHOOSE y \in S: TRUE IN count[x] + SumCount(S \ {x})

\* Processes directly in k and in every job below it.
Subtree(k) == SumCount({j \in Jobs: j >= k})

\* True when admitting one more process into j would exceed some limit on its
\* chain: ground truth, independent of the guard used by the action.
Violates(j) == \E k \in Chain(j): limit[k] # Unset /\ Subtree(k) + 1 > limit[k]

\* The tightest limit on the chain from j to the root.
Effective(j) ==
    LET set == {limit[k]: k \in Chain(j)}
        bounded == {v \in set: v # Unset}
    IN IF bounded = {} THEN Unset
       ELSE CHOOSE m \in bounded: \A v \in bounded: m <= v

TypeOK ==
    /\ limit \in [Jobs -> (0..MaxLimit) \cup {Unset}]
    /\ count \in [Jobs -> 0..MaxCount]
    /\ overAdmitted \in BOOLEAN

Init ==
    /\ limit = [j \in Jobs |-> Unset]
    /\ count = [j \in Jobs |-> 0]
    /\ overAdmitted = FALSE

SetLimit(j, v) ==
    /\ limit' = [limit EXCEPT ![j] = v]
    /\ UNCHANGED <<count, overAdmitted>>

Admit(j) ==
    /\ count[j] < MaxCount
    /\ ~Violates(j) \* admission guard
    /\ count' = [count EXCEPT ![j] = count[j] + 1]
    /\ overAdmitted' = (overAdmitted \/ Violates(j))
    /\ UNCHANGED limit

Reap(j) ==
    /\ count[j] > 0
    /\ count' = [count EXCEPT ![j] = count[j] - 1]
    /\ UNCHANGED <<limit, overAdmitted>>

Next ==
    \/ \E j \in Jobs, v \in (0..MaxLimit) \cup {Unset}: SetLimit(j, v)
    \/ \E j \in Jobs: Admit(j) \/ Reap(j)

Spec == Init /\ [][Next]_vars

\* No admission ever exceeded a limit on its chain.
NoOverAdmission == ~overAdmitted

\* A child is never effectively looser than its parent.
EffectiveWithinParent ==
    \A j \in Jobs \ {0}:
        Effective(Parent(j)) # Unset => (Effective(j) # Unset /\ Effective(j) <= Effective(Parent(j)))

=============================================================================
