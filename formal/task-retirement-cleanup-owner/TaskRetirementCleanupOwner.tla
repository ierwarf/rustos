---------------- MODULE TaskRetirementCleanupOwner ----------------
EXTENDS Naturals

(***************************************************************************
One independently locked user-task cleanup cell, with two monotonic task IDs.
The directory producer publishes last, after terminal metadata. Consumers
release the cell before external cleanup and return an immutable exact token.
This models claim/ACK/reset, not external cleanup success, process/TLB/stack
quiescence, directory locking, arbitrary allocation or global guard removal.
A stale/altered/unclaimed ACK cannot finish another task or permit slot reuse.
***************************************************************************)
Tasks == {1, 2}
VARIABLES task, state, terminalReady, claims, acks, ackTask, ackPayload
vars == <<task, state, terminalReady, claims, acks, ackTask, ackPayload>>

Init ==
    /\ task = 1 /\ state = "vacant" /\ terminalReady = FALSE
    /\ claims = [t \in Tasks |-> 0] /\ acks = [t \in Tasks |-> 0]
    /\ ackTask = 0 /\ ackPayload = FALSE

FinalizeAndPublish ==
    /\ state = "vacant"
    /\ terminalReady' = TRUE
    /\ state' = "pending"
    /\ UNCHANGED <<task, claims, acks, ackTask, ackPayload>>

Claim ==
    /\ state = "pending"
    /\ state' = "claimed"
    /\ claims' = [claims EXCEPT ![task] = @ + 1]
    /\ UNCHANGED <<task, terminalReady, acks, ackTask, ackPayload>>

Ack(returned, exactPayload) ==
    /\ state = "claimed" /\ returned = task /\ exactPayload
    /\ state' = "complete"
    /\ acks' = [acks EXCEPT ![task] = @ + 1]
    /\ ackTask' = returned /\ ackPayload' = exactPayload
    /\ UNCHANGED <<task, terminalReady, claims>>

BadAck(returned, exactPayload) ==
    /\ (state # "claimed" \/ returned # task \/ ~exactPayload)
    /\ UNCHANGED vars

ResetAndReuse ==
    /\ state = "complete" /\ task = 1
    /\ task' = 2 /\ state' = "vacant" /\ terminalReady' = FALSE
    /\ ackTask' = 0 /\ ackPayload' = FALSE
    /\ UNCHANGED <<claims, acks>>

Idle == UNCHANGED vars
Next == FinalizeAndPublish \/ Claim \/ ResetAndReuse \/ Idle
        \/ (\E returned \in Tasks, exactPayload \in BOOLEAN:
              Ack(returned, exactPayload) \/ BadAck(returned, exactPayload))
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ task \in Tasks /\ state \in {"vacant", "pending", "claimed", "complete"}
    /\ terminalReady \in BOOLEAN /\ ackTask \in Tasks \cup {0}
    /\ ackPayload \in BOOLEAN
    /\ claims \in [Tasks -> 0..2] /\ acks \in [Tasks -> 0..2]

NoReuseBeforeExactAck == task = 2 => claims[1] = 1 /\ acks[1] = 1
CompleteRequiresExactClaimedAck ==
    state = "complete" => claims[task] = 1 /\ acks[task] = 1
                           /\ ackTask = task /\ ackPayload
OneClaimAndAckPerTask == \A t \in Tasks: claims[t] <= 1 /\ acks[t] <= 1
PublicationFollowsTerminalMetadata == state # "vacant" => terminalReady
===================================================================
