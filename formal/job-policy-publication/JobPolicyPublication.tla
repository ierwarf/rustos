----------------------- MODULE JobPolicyPublication -----------------------
EXTENDS Naturals

(* Shared bounded publication shape for immutable process membership and a
   reused job's policy word: revoke identity, replace payload, release-commit
   the new incarnation; read exact identity/payload/identity. This SC protocol
   projection is not a weak-memory, Current ownership or teardown proof.
   Loom/Shuttle/herd witnesses own the concrete ordering sensitivity. *)
VARIABLES commit, payload, writer, reader, expected, first, seen, last, accepted
vars == <<commit, payload, writer, reader, expected, first, seen, last, accepted>>

Init ==
    /\ commit = 1 /\ payload = 0
    /\ writer = 0 /\ reader = "Capture"
    /\ expected \in 0..2
    /\ first = 0 /\ seen = 0 /\ last = 0 /\ accepted = FALSE

Revoke ==
    /\ writer = 0
    /\ commit' = 0
    /\ writer' = 1
    /\ UNCHANGED <<payload, reader, expected, first, seen, last, accepted>>

Replace ==
    /\ writer = 1
    /\ payload' = 1 /\ writer' = 2
    /\ UNCHANGED <<commit, reader, expected, first, seen, last, accepted>>

Publish ==
    /\ writer = 2
    /\ commit' = 2 /\ writer' = 3
    /\ UNCHANGED <<payload, reader, expected, first, seen, last, accepted>>

Capture ==
    /\ reader = "Capture"
    /\ first' = commit
    /\ reader' = IF commit = expected /\ commit # 0 THEN "Payload" ELSE "Done"
    /\ UNCHANGED <<commit, payload, writer, expected, seen, last, accepted>>

ReadPayload ==
    /\ reader = "Payload"
    /\ seen' = payload /\ reader' = "Validate"
    /\ UNCHANGED <<commit, payload, writer, expected, first, last, accepted>>

Validate ==
    /\ reader = "Validate"
    /\ last' = commit
    /\ accepted' = (commit = first)
    /\ reader' = "Done"
    /\ UNCHANGED <<commit, payload, writer, expected, first, seen>>

Terminal == writer = 3 /\ reader = "Done" /\ UNCHANGED vars
Next == Revoke \/ Replace \/ Publish \/ Capture \/ ReadPayload \/ Validate \/ Terminal
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ commit \in 0..2 /\ payload \in 0..1 /\ writer \in 0..3
    /\ reader \in {"Capture", "Payload", "Validate", "Done"}
    /\ expected \in 0..2 /\ first \in 0..2 /\ seen \in 0..1 /\ last \in 0..2
    /\ accepted \in BOOLEAN
AcceptedExactInvocation == accepted => first = expected /\ first # 0
AcceptedStableCommit == accepted => last = first
AcceptedExactPayload == accepted => seen = IF expected = 1 THEN 0 ELSE 1

=============================================================================
