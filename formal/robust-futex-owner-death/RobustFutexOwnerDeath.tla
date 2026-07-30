-------------------- MODULE RobustFutexOwnerDeath --------------------
EXTENDS Naturals

(***************************************************************************
Models Linux robust-futex and clear_child_tid cleanup on SMP. The retiring
thread must never publish OWNER_DIED with a read/modify/write sequence: one
atomic compare-exchange publishes the word before wake, and a racing value is
revalidated with a finite retry bound.
***************************************************************************)

CONSTANT RetryLimit

Words == {"owner", "owner-waiters", "foreign", "died", "died-waiters", "zero"}
Modes == {"robust", "clear-tid"}
Idle == "idle"
Validated == "validated"
Observed == "observed"
Published == "published"
Done == "done"
Rejected == "rejected"

VARIABLES mode, phase, word, expected, attempts, woken

vars == <<mode, phase, word, expected, attempts, woken>>

Init ==
    /\ mode \in Modes
    /\ phase = Idle
    /\ word \in {"owner", "owner-waiters"}
    /\ expected = "owner"
    /\ attempts = 0
    /\ woken = FALSE

Validate ==
    /\ phase = Idle
    /\ phase' = Validated
    /\ UNCHANGED <<mode, word, expected, attempts, woken>>

LoadOwner ==
    /\ phase = Validated
    /\ mode = "robust"
    /\ phase' = Observed
    /\ expected' = word
    /\ UNCHANGED <<mode, word, attempts, woken>>

CompetingWrite(next) ==
    /\ phase = Observed
    /\ next \in {"owner", "owner-waiters", "foreign"}
    /\ next # word
    /\ word' = next
    /\ UNCHANGED <<mode, phase, expected, attempts, woken>>

RetryCompareExchange ==
    /\ phase = Observed
    /\ word # expected
    /\ expected \in {"owner", "owner-waiters"}
    /\ attempts < RetryLimit
    /\ expected' = word
    /\ attempts' = attempts + 1
    /\ UNCHANGED <<mode, phase, word, woken>>

PublishOwnerDied ==
    /\ phase = Observed
    /\ word = expected
    /\ expected \in {"owner", "owner-waiters"}
    /\ phase' = Published
    /\ word' = IF expected = "owner-waiters" THEN "died-waiters" ELSE "died"
    /\ UNCHANGED <<mode, expected, attempts, woken>>

RejectForeignOrExhausted ==
    /\ phase = Observed
    /\ expected = "foreign" \/ attempts = RetryLimit
    /\ phase' = Rejected
    /\ UNCHANGED <<mode, word, expected, attempts, woken>>

ClearTidReleaseStore ==
    /\ phase = Validated
    /\ mode = "clear-tid"
    /\ phase' = Published
    /\ word' = "zero"
    /\ UNCHANGED <<mode, expected, attempts, woken>>

Wake ==
    /\ phase = Published
    /\ phase' = Done
    /\ woken' = TRUE
    /\ UNCHANGED <<mode, word, expected, attempts>>

Terminal ==
    /\ phase \in {Done, Rejected}
    /\ UNCHANGED vars

Next ==
    \/ Validate
    \/ LoadOwner
    \/ \E next \in Words: CompetingWrite(next)
    \/ RetryCompareExchange
    \/ PublishOwnerDied
    \/ RejectForeignOrExhausted
    \/ ClearTidReleaseStore
    \/ Wake
    \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ mode \in Modes
    /\ phase \in {Idle, Validated, Observed, Published, Done, Rejected}
    /\ word \in Words
    /\ expected \in Words
    /\ attempts \in 0..RetryLimit
    /\ woken \in BOOLEAN

WakeRequiresAtomicPublication ==
    woken =>
        /\ phase = Done
        /\ IF mode = "robust"
              THEN word \in {"died", "died-waiters"}
              ELSE word = "zero"

OwnerDiedPreservesWaiters ==
    word = "died-waiters" => expected = "owner-waiters"

RetriesAreBounded == attempts <= RetryLimit

=============================================================================
