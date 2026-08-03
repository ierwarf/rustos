--------------------- MODULE PageTableMapTransaction ---------------------
EXTENDS Naturals, FiniteSets, Sequences

(***************************************************************************
A failed mapping call restores the exact pre-call page-table topology. Every
intermediate parent entry is recorded at publication, removed in reverse
ownership order, and its frame is freed only after the one transaction-wide
shootdown has completed.
***************************************************************************)

CONSTANTS Tables

VARIABLES phase, baseTopology, topology, allocated, commitLog,
          mutationPublished, flushCount, framesFreed, failureReturned

vars == <<phase, baseTopology, topology, allocated, commitLog,
          mutationPublished, flushCount, framesFreed, failureReturned>>

Init ==
    /\ phase = "idle"
    /\ baseTopology = {}
    /\ topology = {}
    /\ allocated = {}
    /\ commitLog = <<>>
    /\ mutationPublished = FALSE
    /\ flushCount = 0
    /\ framesFreed = FALSE
    /\ failureReturned = FALSE

Prepare ==
    /\ phase = "idle"
    /\ phase' = "prepared"
    /\ baseTopology' = topology
    /\ allocated' = Tables
    /\ UNCHANGED <<topology, commitLog, mutationPublished, flushCount,
                    framesFreed, failureReturned>>

Publish(table) ==
    /\ phase \in {"prepared", "publishing"}
    /\ table \in allocated \ topology
    /\ phase' = "publishing"
    /\ topology' = topology \cup {table}
    /\ commitLog' = Append(commitLog, table)
    /\ mutationPublished' = TRUE
    /\ UNCHANGED <<baseTopology, allocated, flushCount, framesFreed,
                    failureReturned>>

BeginRollback ==
    /\ phase \in {"prepared", "publishing"}
    /\ phase' = "rolling-back"
    /\ UNCHANGED <<baseTopology, topology, allocated, commitLog,
                    mutationPublished, flushCount, framesFreed,
                    failureReturned>>

RollbackLast ==
    /\ phase = "rolling-back"
    /\ Len(commitLog) > 0
    /\ LET last == commitLog[Len(commitLog)] IN
       /\ topology' = topology \ {last}
       /\ commitLog' = SubSeq(commitLog, 1, Len(commitLog) - 1)
    /\ UNCHANGED <<phase, baseTopology, allocated, mutationPublished,
                    flushCount, framesFreed, failureReturned>>

FinishRollback ==
    /\ phase = "rolling-back"
    /\ Len(commitLog) = 0
    /\ topology = baseTopology
    /\ phase' = "failed"
    /\ flushCount' = IF mutationPublished THEN 1 ELSE 0
    /\ framesFreed' = TRUE
    /\ failureReturned' = TRUE
    /\ UNCHANGED <<baseTopology, topology, allocated, commitLog,
                    mutationPublished>>

Commit ==
    /\ phase \in {"prepared", "publishing"}
    /\ phase' = "committed"
    /\ allocated' = {}
    /\ commitLog' = <<>>
    /\ UNCHANGED <<baseTopology, topology, mutationPublished, flushCount,
                    framesFreed, failureReturned>>

Terminal ==
    /\ phase \in {"failed", "committed"}
    /\ UNCHANGED vars

Next == Prepare \/ BeginRollback \/ RollbackLast \/ FinishRollback \/ Commit
        \/ Terminal \/ \E table \in Tables: Publish(table)

Spec == Init /\ [][Next]_vars /\ WF_vars(RollbackLast) /\ WF_vars(FinishRollback)

TypeOK ==
    /\ phase \in {"idle", "prepared", "publishing", "rolling-back",
                   "failed", "committed"}
    /\ baseTopology \subseteq Tables
    /\ topology \subseteq Tables
    /\ allocated \subseteq Tables
    /\ \A index \in 1..Len(commitLog): commitLog[index] \in Tables
    /\ mutationPublished \in BOOLEAN
    /\ flushCount \in 0..1
    /\ framesFreed \in BOOLEAN
    /\ failureReturned \in BOOLEAN

FailureIsTopologyNeutral == failureReturned => topology = baseTopology
FailureReturnsAfterRollback == failureReturned => Len(commitLog) = 0
ReclaimFollowsSingleFlush ==
    framesFreed /\ mutationPublished => flushCount = 1
NoEarlyIntermediateReclaim ==
    phase \in {"prepared", "publishing", "rolling-back"} => ~framesFreed
OneTransactionOneFlush == flushCount <= 1
PublishedTopologyHasRollbackCustody ==
    phase \in {"publishing", "rolling-back"} =>
        topology \ baseTopology = {commitLog[index] : index \in 1..Len(commitLog)}
RollbackEventuallyTerminates == phase = "rolling-back" ~> phase = "failed"

=============================================================================
