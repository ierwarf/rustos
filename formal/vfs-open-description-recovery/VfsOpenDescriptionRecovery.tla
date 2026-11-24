------------------ MODULE VfsOpenDescriptionRecovery ------------------
EXTENDS Integers, TLC

(***************************************************************************
Finite refinement of the rootd-retained vfsd open-description protocol.
An OPEN is published only after every path chunk is durable.
For regular files (kind = "file"), the kernel owns the live sequential cursor
(kernelCursor), issues stateless positioned reads, advances kernelCursor only
after user-copy succeeds, and flushes durableCursor at explicit settle points
(lseek/write/fsync/CURSOR_SETTLE_SYNC). For directories (kind = "dir"),
getdents64 retains prepared/commit/cancel. CLOSE retains a tombstone through
response loss and only an explicit visibility ACK permits compaction.
***************************************************************************)

CONSTANT MaxCursor
ASSUME MaxCursor \in Nat \ {0}

Kinds == {"file", "dir"}
States == {"absent", "staging", "stable", "prepared", "tombstone"}

VARIABLES kind, serviceLive, endpointPublished, durable, local, pathChunks,
          durableCursor, kernelCursor, startCursor, deliveredOffset,
          copyVisible, closeReplySeen, compacted

vars == <<kind, serviceLive, endpointPublished, durable, local, pathChunks,
          durableCursor, kernelCursor, startCursor, deliveredOffset,
          copyVisible, closeReplySeen, compacted>>

Init ==
    /\ kind \in Kinds
    /\ serviceLive = TRUE
    /\ endpointPublished = FALSE
    /\ durable = "absent"
    /\ local = "absent"
    /\ pathChunks = 0
    /\ durableCursor = 0
    /\ kernelCursor = 0
    /\ startCursor = 0
    /\ deliveredOffset = 0
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
    /\ UNCHANGED <<kind, serviceLive, endpointPublished, durableCursor,
                   kernelCursor, startCursor, deliveredOffset,
                   copyVisible, closeReplySeen, compacted>>

WritePathChunk ==
    /\ serviceLive
    /\ durable = "staging"
    /\ pathChunks < 2
    /\ pathChunks' = pathChunks + 1
    /\ UNCHANGED <<kind, serviceLive, endpointPublished, durable, local,
                   durableCursor, kernelCursor, startCursor, deliveredOffset,
                   copyVisible, closeReplySeen, compacted>>

PublishOpen ==
    /\ serviceLive
    /\ durable = "staging"
    /\ pathChunks = 2
    /\ durable' = "stable"
    /\ local' = "stable"
    /\ UNCHANGED <<kind, serviceLive, endpointPublished, pathChunks,
                   durableCursor, kernelCursor, startCursor, deliveredOffset,
                   copyVisible, closeReplySeen, compacted>>

CancelStagedOpen ==
    /\ serviceLive
    /\ durable = "staging"
    /\ durable' = "tombstone"
    /\ local' = "absent"
    /\ closeReplySeen' = FALSE
    /\ UNCHANGED <<kind, serviceLive, endpointPublished, pathChunks,
                   durableCursor, kernelCursor, startCursor, deliveredOffset,
                   copyVisible, compacted>>

PublishEndpoint ==
    /\ serviceLive
    /\ ~endpointPublished
    /\ local = IF durable \in {"stable", "prepared"} THEN durable ELSE "absent"
    /\ endpointPublished' = TRUE
    /\ UNCHANGED <<kind, serviceLive, durable, local, pathChunks,
                   durableCursor, kernelCursor, startCursor, deliveredOffset,
                   copyVisible, closeReplySeen, compacted>>

\* Regular-file stateless positioned read: succeeds only after user copy.
StatelessFileReadSuccess ==
    /\ kind = "file"
    /\ serviceLive
    /\ endpointPublished
    /\ durable = "stable"
    /\ kernelCursor < MaxCursor
    /\ deliveredOffset' = kernelCursor
    /\ kernelCursor' = kernelCursor + 1
    /\ copyVisible' = TRUE
    /\ UNCHANGED <<kind, serviceLive, endpointPublished, durable, local,
                   pathChunks, durableCursor, startCursor, closeReplySeen,
                   compacted>>

\* Regular-file positioned read where copy_to_user faults or reply is lost.
StatelessFileReadFaultOrLoss ==
    /\ kind = "file"
    /\ serviceLive
    /\ endpointPublished
    /\ durable = "stable"
    /\ kernelCursor < MaxCursor
    /\ copyVisible' = FALSE
    /\ UNCHANGED <<kind, serviceLive, endpointPublished, durable, local,
                   pathChunks, durableCursor, kernelCursor, startCursor,
                   deliveredOffset, closeReplySeen, compacted>>

\* Explicit durable settle (CURSOR_SETTLE_SYNC, lseek, write, fsync).
SettleFileCursor ==
    /\ kind = "file"
    /\ serviceLive
    /\ endpointPublished
    /\ durable = "stable"
    /\ durableCursor # kernelCursor
    /\ durableCursor' = kernelCursor
    /\ UNCHANGED <<kind, serviceLive, endpointPublished, durable, local,
                   pathChunks, kernelCursor, startCursor, deliveredOffset,
                   copyVisible, closeReplySeen, compacted>>

\* Directory getdents64 retains prepare/commit/cancel.
PrepareDirRead ==
    /\ kind = "dir"
    /\ serviceLive
    /\ endpointPublished
    /\ durable = "stable"
    /\ durableCursor < MaxCursor
    /\ startCursor' = durableCursor
    /\ durableCursor' = durableCursor + 1
    /\ durable' = "prepared"
    /\ local' = "prepared"
    /\ copyVisible' = FALSE
    /\ UNCHANGED <<kind, serviceLive, endpointPublished, pathChunks,
                   kernelCursor, deliveredOffset, closeReplySeen, compacted>>

CommitDirRead ==
    /\ kind = "dir"
    /\ durable = "prepared"
    /\ copyVisible' = TRUE
    /\ deliveredOffset' = startCursor
    /\ kernelCursor' = durableCursor
    /\ durable' = "stable"
    /\ local' = "stable"
    /\ UNCHANGED <<kind, serviceLive, endpointPublished, pathChunks,
                   durableCursor, startCursor, closeReplySeen, compacted>>

CancelDirRead ==
    /\ kind = "dir"
    /\ durable = "prepared"
    /\ durableCursor' = startCursor
    /\ durable' = "stable"
    /\ local' = "stable"
    /\ copyVisible' = FALSE
    /\ UNCHANGED <<kind, serviceLive, endpointPublished, pathChunks,
                   kernelCursor, deliveredOffset, startCursor,
                   closeReplySeen, compacted>>

CloseDescription ==
    /\ serviceLive
    /\ endpointPublished
    /\ durable = "stable"
    /\ durable' = "tombstone"
    /\ local' = "absent"
    /\ closeReplySeen' = FALSE
    /\ UNCHANGED <<kind, serviceLive, endpointPublished, pathChunks,
                   durableCursor, kernelCursor, startCursor, deliveredOffset,
                   copyVisible, compacted>>

ObserveCloseReply ==
    /\ durable = "tombstone"
    /\ closeReplySeen' = TRUE
    /\ UNCHANGED <<kind, serviceLive, endpointPublished, durable, local,
                   pathChunks, durableCursor, kernelCursor, startCursor,
                   deliveredOffset, copyVisible, compacted>>

CompactTombstone ==
    /\ durable = "tombstone"
    /\ closeReplySeen
    /\ durable' = "absent"
    /\ compacted' = TRUE
    /\ UNCHANGED <<kind, serviceLive, endpointPublished, local, pathChunks,
                   durableCursor, kernelCursor, startCursor, deliveredOffset,
                   copyVisible, closeReplySeen>>

Crash ==
    /\ serviceLive
    /\ serviceLive' = FALSE
    /\ endpointPublished' = FALSE
    /\ local' = "absent"
    /\ UNCHANGED <<kind, durable, pathChunks, durableCursor, kernelCursor,
                   startCursor, deliveredOffset, copyVisible,
                   closeReplySeen, compacted>>

Recover ==
    /\ ~serviceLive
    /\ serviceLive' = TRUE
    /\ endpointPublished' = FALSE
    /\ local' = IF durable \in {"stable", "prepared"} THEN durable ELSE "absent"
    /\ UNCHANGED <<kind, durable, pathChunks, durableCursor, kernelCursor,
                   startCursor, deliveredOffset, copyVisible,
                   closeReplySeen, compacted>>

Terminal ==
    /\ compacted
    /\ UNCHANGED vars

Next == StageOpen \/ WritePathChunk \/ PublishOpen \/ CancelStagedOpen \/ PublishEndpoint
        \/ StatelessFileReadSuccess \/ StatelessFileReadFaultOrLoss \/ SettleFileCursor
        \/ PrepareDirRead \/ CommitDirRead \/ CancelDirRead \/ CloseDescription
        \/ ObserveCloseReply \/ CompactTombstone \/ Crash \/ Recover \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ kind \in Kinds
    /\ serviceLive \in BOOLEAN
    /\ endpointPublished \in BOOLEAN
    /\ durable \in States
    /\ local \in States
    /\ pathChunks \in 0..2
    /\ durableCursor \in 0..MaxCursor
    /\ kernelCursor \in 0..MaxCursor
    /\ startCursor \in 0..MaxCursor
    /\ deliveredOffset \in 0..MaxCursor
    /\ copyVisible \in BOOLEAN
    /\ closeReplySeen \in BOOLEAN
    /\ compacted \in BOOLEAN

CompletePathBeforeLive == durable \in {"stable", "prepared"} => pathChunks = 2

FileNeverEntersPrepared == kind = "file" => durable # "prepared"

FileCursorMonotonicAndExact ==
    kind = "file" =>
        /\ durableCursor <= kernelCursor
        /\ copyVisible => deliveredOffset + 1 = kernelCursor

PreparedAdvanceIsReversible ==
    durable = "prepared" => kind = "dir" /\ durableCursor = startCursor + 1

EndpointPublishesOnlyReplayedState ==
    endpointPublished => serviceLive
        /\ local = IF durable \in {"stable", "prepared"} THEN durable ELSE "absent"

CompactionRequiresObservedReply == compacted => closeReplySeen

TombstoneSurvivesUntilAck == durable = "tombstone" /\ ~closeReplySeen => ~compacted

=============================================================================
