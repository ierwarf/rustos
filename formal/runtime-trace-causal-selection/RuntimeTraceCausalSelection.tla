-------------------- MODULE RuntimeTraceCausalSelection --------------------
EXTENDS Naturals, Sequences, FiniteSets

(* A bounded projection of immutable, matching log candidates. The production
   reader verifies complete frames and identity evidence before considering a
   candidate's timestamp. It selects the first verified candidate after ALL
   required predecessors, not necessarily the first occurrence of its marker.
   This does not model port I/O, clock correctness, live logging or file reads. *)

VARIABLES events, firstParent, secondParent, dependenciesPresent,
          cursor, status, chosen
vars == <<events, firstParent, secondParent, dependenciesPresent,
          cursor, status, chosen>>

Bound == IF firstParent >= secondParent THEN firstParent ELSE secondParent
Verified(i) == events[i].framed /\ events[i].identity
(* The expected terminal decision does not reuse the implementation's Bound
   or Verified helpers: weakening either must falsify these invariants. *)
ExpectedEligible(i) ==
    /\ \A j \in 1..i : events[j].framed /\ events[j].identity
    /\ events[i].time >= firstParent
    /\ events[i].time >= secondParent
Candidates == {i \in 1..Len(events) : ExpectedEligible(i)}

Init ==
    /\ events \in [1..3 -> [time : 0..2, framed : BOOLEAN, identity : BOOLEAN]]
    /\ firstParent \in 0..1
    /\ secondParent \in 0..1
    /\ dependenciesPresent \in BOOLEAN
    /\ cursor = 1
    /\ chosen = 0
    /\ status = IF dependenciesPresent THEN "Scanning" ELSE "Rejected"

Inspect ==
    /\ status = "Scanning"
    /\ cursor <= Len(events)
    /\ IF ~Verified(cursor)
          THEN /\ status' = "Rejected"
               /\ chosen' = 0
          ELSE IF events[cursor].time >= Bound
               THEN /\ status' = "Accepted"
                    /\ chosen' = cursor
               ELSE /\ status' = IF cursor = Len(events) THEN "Rejected" ELSE "Scanning"
                    /\ chosen' = 0
    /\ cursor' = cursor + 1
    /\ UNCHANGED <<events, firstParent, secondParent, dependenciesPresent>>

Terminal == status # "Scanning" /\ UNCHANGED vars
Next == Inspect \/ Terminal
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ events \in [1..3 -> [time : 0..2, framed : BOOLEAN, identity : BOOLEAN]]
    /\ firstParent \in 0..1 /\ secondParent \in 0..1
    /\ dependenciesPresent \in BOOLEAN
    /\ cursor \in 1..4
    /\ status \in {"Scanning", "Accepted", "Rejected"}
    /\ chosen \in 0..3

AcceptedAuthority ==
    status = "Accepted" =>
        /\ dependenciesPresent
        /\ chosen \in Candidates
        /\ \A i \in 1..chosen : events[i].framed /\ events[i].identity
        /\ \A i \in 1..(chosen - 1) :
               events[i].time < firstParent \/ events[i].time < secondParent

TerminalDecision ==
    status = "Rejected" => (~dependenciesPresent \/ Candidates = {})

=============================================================================
