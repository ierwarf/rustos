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
NoKey == "none"
PrivateKey == "private"
SharedKey == "shared"

VARIABLES mode, phase, word, expected, attempts, woken, stableShared,
          waiterKey, wakeKey

vars == <<mode, phase, word, expected, attempts, woken, stableShared,
          waiterKey, wakeKey>>

Init ==
    /\ mode \in Modes
    /\ phase = Idle
    /\ word \in {"owner", "owner-waiters"}
    /\ expected = "owner"
    /\ attempts = 0
    /\ woken = FALSE
    /\ stableShared \in BOOLEAN
    /\ waiterKey = IF stableShared THEN SharedKey ELSE PrivateKey
    /\ wakeKey = NoKey

Validate ==
    /\ phase = Idle
    /\ phase' = Validated
    /\ UNCHANGED <<mode, word, expected, attempts, woken, stableShared,
                    waiterKey, wakeKey>>

LoadOwner ==
    /\ phase = Validated
    /\ mode = "robust"
    /\ phase' = Observed
    /\ expected' = word
    /\ UNCHANGED <<mode, word, attempts, woken, stableShared, waiterKey,
                    wakeKey>>

CompetingWrite(next) ==
    /\ phase = Observed
    /\ next \in {"owner", "owner-waiters", "foreign"}
    /\ next # word
    /\ word' = next
    /\ UNCHANGED <<mode, phase, expected, attempts, woken, stableShared,
                    waiterKey, wakeKey>>

RetryCompareExchange ==
    /\ phase = Observed
    /\ word # expected
    /\ expected \in {"owner", "owner-waiters"}
    /\ attempts < RetryLimit
    /\ expected' = word
    /\ attempts' = attempts + 1
    /\ UNCHANGED <<mode, phase, word, woken, stableShared, waiterKey, wakeKey>>

PublishOwnerDied ==
    /\ phase = Observed
    /\ word = expected
    /\ expected \in {"owner", "owner-waiters"}
    /\ phase' = Published
    /\ word' = IF expected = "owner-waiters" THEN "died-waiters" ELSE "died"
    /\ UNCHANGED <<mode, expected, attempts, woken, stableShared, waiterKey,
                    wakeKey>>

RejectForeignOrExhausted ==
    /\ phase = Observed
    /\ expected = "foreign" \/ attempts = RetryLimit
    /\ phase' = Rejected
    /\ UNCHANGED <<mode, word, expected, attempts, woken, stableShared,
                    waiterKey, wakeKey>>

ClearTidReleaseStore ==
    /\ phase = Validated
    /\ mode = "clear-tid"
    /\ phase' = Published
    /\ word' = "zero"
    /\ UNCHANGED <<mode, expected, attempts, woken, stableShared, waiterKey,
                    wakeKey>>

Wake ==
    /\ phase = Published
    /\ phase' = Done
    /\ woken' = TRUE
    /\ wakeKey' = waiterKey
    /\ UNCHANGED <<mode, word, expected, attempts, stableShared, waiterKey>>

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
    /\ stableShared \in BOOLEAN
    /\ waiterKey \in {PrivateKey, SharedKey}
    /\ wakeKey \in {NoKey, PrivateKey, SharedKey}

WakeRequiresAtomicPublication ==
    woken =>
        /\ phase = Done
        /\ IF mode = "robust"
              THEN word \in {"died", "died-waiters"}
              ELSE word = "zero"

OwnerDiedPreservesWaiters ==
    word = "died-waiters" => expected = "owner-waiters"

RetriesAreBounded == attempts <= RetryLimit
NonPrivateAnonymousFallsBackToPrivate == ~stableShared => waiterKey = PrivateKey
KernelGeneratedWakeUsesEquivalentKey == woken => wakeKey = waiterKey

=============================================================================
