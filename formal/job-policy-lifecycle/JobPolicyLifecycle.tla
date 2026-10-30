------------------------ MODULE JobPolicyLifecycle ------------------------
EXTENDS Naturals, FiniteSets

(* One cached operation word, four fixed jobs (root, parent, child, sibling).
   Writers serialize under the process-table owner; individual descendant
   publication may interleave with readers, but a completed call must refresh
   all descendants. cached is the exposed ceiling (empty when its incarnation
   is invalid), not revoked raw storage. Whole masks/subtrees are not atomic
   snapshots. job-policy-publication covers physical payload interleavings;
   existing job-lifecycle owns member/object/page retirement before reuse. *)
Jobs == 0..3
Bits == {0, 1}
NoJob == 4
Ancestors(j) == CASE j = 0 -> {0} [] j = 1 -> {0, 1}
                   [] j = 2 -> {0, 1, 2} [] OTHER -> {0, 3}
Descendants(j) == {k \in Jobs : j \in Ancestors(k)}
Parent(j) == IF j = 2 THEN 1 ELSE 0
Computed(ownMap, stateMap, j) ==
    {b \in Bits : \A a \in Ancestors(j) : stateMap[a] = "Live" /\ b \in ownMap[a]}

VARIABLES own, state, generation, cached, publishedGeneration, writer, pending,
          sets, kills, setRight, limitsRight, lastBirth, readAllowed,
          readGeneration, readActualGeneration, readPublishedGeneration
vars == <<own, state, generation, cached, publishedGeneration, writer, pending,
          sets, kills, setRight, limitsRight, lastBirth, readAllowed,
          readGeneration, readActualGeneration, readPublishedGeneration>>

Init ==
    /\ own = [j \in Jobs |-> Bits]
    /\ state = [j \in Jobs |-> "Live"]
    /\ generation = [j \in Jobs |-> 1]
    /\ cached = [j \in Jobs |-> Bits]
    /\ publishedGeneration = [j \in Jobs |-> 1]
    /\ writer = "None" /\ pending = {}
    /\ sets = 0 /\ kills = 0
    /\ setRight \in BOOLEAN /\ limitsRight \in BOOLEAN
    /\ lastBirth = NoJob
    /\ readAllowed = FALSE
    /\ readGeneration = 0 /\ readActualGeneration = 0 /\ readPublishedGeneration = 0

BeginSet ==
    /\ writer = "None" /\ sets = 0
    /\ setRight \* SET_LIMITS cannot authorize a qualitative policy write
    /\ \E j \in Jobs, mask \in SUBSET Bits :
         /\ state[j] = "Live"
         /\ own' = [own EXCEPT ![j] = mask]
         /\ pending' = {k \in Descendants(j) : state[k] # "Dead"}
    /\ writer' = "Set" /\ sets' = 1 /\ lastBirth' = NoJob
    /\ UNCHANGED <<state, generation, cached, publishedGeneration, kills,
                   setRight, limitsRight, readAllowed, readGeneration,
                   readActualGeneration, readPublishedGeneration>>

BeginKill ==
    /\ writer = "None" /\ kills = 0
    /\ \E j \in Jobs \ {0} :
         /\ state[j] = "Live"
         /\ state' = [k \in Jobs |-> IF k \in Descendants(j) THEN "Killing" ELSE state[k]]
         /\ pending' = Descendants(j)
    /\ writer' = "Kill" /\ kills' = 1 /\ lastBirth' = NoJob
    /\ UNCHANGED <<own, generation, cached, publishedGeneration, sets,
                   setRight, limitsRight, readAllowed, readGeneration,
                   readActualGeneration, readPublishedGeneration>>

Publish ==
    /\ writer # "None" /\ pending # {}
    /\ \E j \in pending :
         /\ cached' = [cached EXCEPT ![j] = Computed(own, state, j)]
         /\ publishedGeneration' = [publishedGeneration EXCEPT
                 ![j] = IF state[j] = "Live" THEN generation[j] ELSE 0]
         /\ pending' = pending \ {j}
    /\ writer' = IF pending' = {} THEN "None" ELSE writer
    /\ lastBirth' = NoJob
    /\ UNCHANGED <<own, state, generation, sets, kills, setRight, limitsRight,
                   readAllowed, readGeneration, readActualGeneration, readPublishedGeneration>>

ReapEmptyJob ==
    /\ writer = "None"
    /\ \E j \in Jobs \ {0} :
         /\ state[j] = "Killing"
         /\ \A k \in Descendants(j) \ {j} : state[k] = "Dead"
         /\ state' = [state EXCEPT ![j] = "Dead"]
    /\ lastBirth' = NoJob
    /\ UNCHANGED <<own, generation, cached, publishedGeneration, writer, pending,
                   sets, kills, setRight, limitsRight, readAllowed,
                   readGeneration, readActualGeneration, readPublishedGeneration>>

Recreate ==
    /\ writer = "None"
    /\ \E j \in Jobs \ {0} :
         /\ state[j] = "Dead" /\ state[Parent(j)] = "Live" /\ generation[j] < 2
         /\ own' = [own EXCEPT ![j] = Bits]
         /\ state' = [state EXCEPT ![j] = "Live"]
         /\ generation' = [generation EXCEPT ![j] = @ + 1]
         /\ cached' = [cached EXCEPT ![j] = Computed(own', state', j)]
         /\ publishedGeneration' = [publishedGeneration EXCEPT ![j] = generation'[j]]
         /\ lastBirth' = j
    /\ UNCHANGED <<writer, pending, sets, kills, setRight, limitsRight,
                   readAllowed, readGeneration, readActualGeneration, readPublishedGeneration>>

Observe ==
    /\ \E j \in Jobs, requested \in 0..2, bit \in Bits :
         /\ readAllowed' = (requested # 0 /\ requested = publishedGeneration[j] /\ bit \in cached[j])
         /\ readGeneration' = requested
         /\ readActualGeneration' = generation[j]
         /\ readPublishedGeneration' = publishedGeneration[j]
    /\ lastBirth' = NoJob
    /\ UNCHANGED <<own, state, generation, cached, publishedGeneration, writer,
                   pending, sets, kills, setRight, limitsRight>>

Next == BeginSet \/ BeginKill \/ Publish \/ ReapEmptyJob \/ Recreate \/ Observe
Spec == Init /\ [][Next]_vars

(* Independent expected tree, not the implementation's Ancestors/Computed. *)
ExpectedRoot == IF state[0] = "Live" THEN own[0] ELSE {}
ExpectedParent == IF state[1] = "Live" THEN own[1] \cap ExpectedRoot ELSE {}
ExpectedChild == IF state[2] = "Live" THEN own[2] \cap ExpectedParent ELSE {}
ExpectedSibling == IF state[3] = "Live" THEN own[3] \cap ExpectedRoot ELSE {}
Expected == [j \in Jobs |-> CASE j = 0 -> ExpectedRoot [] j = 1 -> ExpectedParent
                           [] j = 2 -> ExpectedChild [] OTHER -> ExpectedSibling]

TypeOK ==
    /\ own \in [Jobs -> SUBSET Bits] /\ cached \in [Jobs -> SUBSET Bits]
    /\ state \in [Jobs -> {"Live", "Killing", "Dead"}]
    /\ generation \in [Jobs -> 1..2] /\ publishedGeneration \in [Jobs -> 0..2]
    /\ writer \in {"None", "Set", "Kill"} /\ pending \subseteq Jobs
    /\ sets \in 0..1 /\ kills \in 0..1
    /\ setRight \in BOOLEAN /\ limitsRight \in BOOLEAN
    /\ lastBirth \in Jobs \cup {NoJob} /\ readAllowed \in BOOLEAN
    /\ readGeneration \in 0..2 /\ readActualGeneration \in 0..2
    /\ readPublishedGeneration \in 0..2

CompletedCacheIsExact ==
    writer = "None" =>
        /\ cached = Expected
        /\ \A j \in Jobs : publishedGeneration[j] = IF state[j] = "Live" THEN generation[j] ELSE 0
SetRequiresItsOwnRight == writer = "Set" => setRight
BirthResetsOwnCeiling == lastBirth # NoJob => own[lastBirth] = Bits
ReadDoesNotAliasGeneration ==
    readAllowed => readGeneration = readActualGeneration /\ readPublishedGeneration # 0

=============================================================================
