------------------ MODULE VfsOpenDescriptionRecovery ------------------
EXTENDS Integers, TLC

(***************************************************************************
Finite refinement of the rootd-retained vfsd open-description protocol.
An OPEN is published only after every path chunk is durable. Sequential I/O
first prepares a cursor advance and is then committed or cancelled after the
kernel user-copy result. CLOSE retains a tombstone through response loss and
only an explicit visibility ACK permits compaction.
***************************************************************************)

CONSTANT MaxCursor
ASSUME MaxCursor \in Nat \ {0}

States == {"absent", "staging", "stable", "prepared", "tombstone"}

VARIABLES serviceLive, endpointPublished, durable, local, pathChunks,
          cursor, startCursor, copyVisible, closeReplySeen, compacted

vars == <<serviceLive, endpointPublished, durable, local, pathChunks,
          cursor, startCursor, copyVisible, closeReplySeen, compacted>>

Init ==
    /\ serviceLive = TRUE
    /\ endpointPublished = FALSE
    /\ durable = "absent"
    /\ local = "absent"
    /\ pathChunks = 0
    /\ cursor = 0
    /\ startCursor = 0
    /\ copyVisible = FALSE
    /\ closeReplySeen = FALSE
    /\ compacted = FALSE

StageOpen ==
    /\ serviceLive
    /\ ~compacted
    /\ durable = "absent"
    /\ durable' = "staging"
    /\ local' = "absent"
    /\ pathChunks' = 0
    /\ UNCHANGED <<serviceLive, endpointPublished, cursor, startCursor,
                   copyVisible, closeReplySeen, compacted>>

WritePathChunk ==
    /\ serviceLive
    /\ durable = "staging"
    /\ pathChunks < 2
    /\ pathChunks' = pathChunks + 1
    /\ UNCHANGED <<serviceLive, endpointPublished, durable, local, cursor,
                   startCursor, copyVisible, closeReplySeen, compacted>>

PublishOpen ==
    /\ serviceLive
    /\ durable = "staging"
    /\ pathChunks = 2
    /\ durable' = "stable"
    /\ local' = "stable"
    /\ UNCHANGED <<serviceLive, endpointPublished, pathChunks, cursor,
                   startCursor, copyVisible, closeReplySeen, compacted>>

CancelStagedOpen ==
    /\ serviceLive
    /\ durable = "staging"
    /\ durable' = "tombstone"
    /\ local' = "absent"
    /\ closeReplySeen' = FALSE
    /\ UNCHANGED <<serviceLive, endpointPublished, pathChunks, cursor,
                   startCursor, copyVisible, compacted>>

PublishEndpoint ==
    /\ serviceLive
    /\ ~endpointPublished
    /\ local = IF durable \in {"stable", "prepared"} THEN durable ELSE "absent"
    /\ endpointPublished' = TRUE
    /\ UNCHANGED <<serviceLive, durable, local, pathChunks, cursor,
                   startCursor, copyVisible, closeReplySeen, compacted>>

PrepareRead ==
    /\ serviceLive
    /\ endpointPublished
    /\ durable = "stable"
    /\ cursor < MaxCursor
    /\ startCursor' = cursor
    /\ cursor' = cursor + 1
    /\ durable' = "prepared"
    /\ local' = "prepared"
    /\ copyVisible' = FALSE
    /\ UNCHANGED <<serviceLive, endpointPublished, pathChunks,
                   closeReplySeen, compacted>>

CommitRead ==
    /\ durable = "prepared"
    /\ copyVisible' = TRUE
    /\ durable' = "stable"
    /\ local' = "stable"
    /\ UNCHANGED <<serviceLive, endpointPublished, pathChunks, cursor,
                   startCursor, closeReplySeen, compacted>>

CancelRead ==
    /\ durable = "prepared"
    /\ cursor' = startCursor
    /\ durable' = "stable"
    /\ local' = "stable"
    /\ copyVisible' = FALSE
    /\ UNCHANGED <<serviceLive, endpointPublished, pathChunks, startCursor,
                   closeReplySeen, compacted>>

CloseDescription ==
    /\ serviceLive
    /\ endpointPublished
    /\ durable = "stable"
    /\ durable' = "tombstone"
    /\ local' = "absent"
    /\ closeReplySeen' = FALSE
    /\ UNCHANGED <<serviceLive, endpointPublished, pathChunks, cursor,
                   startCursor, copyVisible, compacted>>

ObserveCloseReply ==
    /\ durable = "tombstone"
    /\ closeReplySeen' = TRUE
    /\ UNCHANGED <<serviceLive, endpointPublished, durable, local,
                   pathChunks, cursor, startCursor, copyVisible, compacted>>

CompactTombstone ==
    /\ durable = "tombstone"
    /\ closeReplySeen
    /\ durable' = "absent"
    /\ compacted' = TRUE
    /\ UNCHANGED <<serviceLive, endpointPublished, local, pathChunks,
                   cursor, startCursor, copyVisible, closeReplySeen>>

Crash ==
    /\ serviceLive
    /\ serviceLive' = FALSE
    /\ endpointPublished' = FALSE
    /\ local' = "absent"
    /\ UNCHANGED <<durable, pathChunks, cursor, startCursor, copyVisible,
                   closeReplySeen, compacted>>

Recover ==
    /\ ~serviceLive
    /\ serviceLive' = TRUE
    /\ endpointPublished' = FALSE
    /\ local' = IF durable \in {"stable", "prepared"} THEN durable ELSE "absent"
    /\ UNCHANGED <<durable, pathChunks, cursor, startCursor, copyVisible,
                   closeReplySeen, compacted>>

Terminal ==
    /\ compacted
    /\ UNCHANGED vars

Next == StageOpen \/ WritePathChunk \/ PublishOpen \/ CancelStagedOpen \/ PublishEndpoint
        \/ PrepareRead \/ CommitRead \/ CancelRead \/ CloseDescription
        \/ ObserveCloseReply \/ CompactTombstone \/ Crash \/ Recover \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ serviceLive \in BOOLEAN
    /\ endpointPublished \in BOOLEAN
    /\ durable \in States
    /\ local \in States
    /\ pathChunks \in 0..2
    /\ cursor \in 0..MaxCursor
    /\ startCursor \in 0..MaxCursor
    /\ copyVisible \in BOOLEAN
    /\ closeReplySeen \in BOOLEAN
    /\ compacted \in BOOLEAN

CompletePathBeforeLive == durable \in {"stable", "prepared"} => pathChunks = 2

PreparedAdvanceIsReversible == durable = "prepared" => cursor = startCursor + 1

EndpointPublishesOnlyReplayedState ==
    endpointPublished => serviceLive
        /\ local = IF durable \in {"stable", "prepared"} THEN durable ELSE "absent"

CompactionRequiresObservedReply == compacted => closeReplySeen

TombstoneSurvivesUntilAck == durable = "tombstone" /\ ~closeReplySeen => ~compacted

=============================================================================
