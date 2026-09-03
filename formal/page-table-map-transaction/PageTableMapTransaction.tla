--------------------- MODULE PageTableMapTransaction ---------------------
EXTENDS Naturals, FiniteSets, Sequences

(***************************************************************************
A mapping transaction owns its leaves and only its leaves.

Intermediate tables are published by an atomic compare-and-swap that a fault
on another CPU may win, because `mmap` no longer builds tables for the range
it reserves and the exception path builds the ones it needs. The mutation
guard excludes other normal-time writers; it does not exclude that installer.

Two consequences are modelled here. A failed transaction restores the exact
leaf topology but keeps every intermediate table it published: withdrawing one
is no longer provable, since a concurrent installer may already have put a leaf
inside it. And every reachable table is claimed by exactly one ownership
ledger - the transaction's `owned_frames`, or the fault path's page-table tag -
which is the disagreement address-space retirement must fail stop on.
***************************************************************************)

CONSTANTS Tables, Leaves

VARIABLES phase, baseLeaves, leaves, leafLog, topology, ledger, faultLedger,
          mutationPublished, flushCount, leafFramesFreed, tableFramesFreed,
          failureReturned

vars == <<phase, baseLeaves, leaves, leafLog, topology, ledger, faultLedger,
          mutationPublished, flushCount, leafFramesFreed, tableFramesFreed,
          failureReturned>>

Init ==
    /\ phase = "idle"
    /\ baseLeaves = {}
    /\ leaves = {}
    /\ leafLog = <<>>
    /\ topology = {}
    /\ ledger = {}
    /\ faultLedger = {}
    /\ mutationPublished = FALSE
    /\ flushCount = 0
    /\ leafFramesFreed = FALSE
    /\ tableFramesFreed = FALSE
    /\ failureReturned = FALSE

Prepare ==
    /\ phase = "idle"
    /\ phase' = "prepared"
    /\ baseLeaves' = leaves
    /\ UNCHANGED <<leaves, leafLog, topology, ledger, faultLedger,
                    mutationPublished, flushCount, leafFramesFreed,
                    tableFramesFreed, failureReturned>>

(* The transaction wins the publication CAS for one absent table. *)
InstallTable(table) ==
    /\ phase \in {"prepared", "publishing"}
    /\ table \in Tables \ topology
    /\ phase' = "publishing"
    /\ topology' = topology \cup {table}
    /\ ledger' = ledger \cup {table}
    /\ UNCHANGED <<baseLeaves, leaves, leafLog, faultLedger,
                    mutationPublished, flushCount, leafFramesFreed,
                    tableFramesFreed, failureReturned>>

(* An exception-time installer on another CPU wins the same kind of CAS. It
   takes no lock and observes no phase, so it may run at any point. Requiring
   the entry to be absent is the whole content of the CAS: whichever side takes
   this step, the other can no longer take it for that table. *)
FaultInstallTable(table) ==
    /\ phase # "retired"
    /\ table \in Tables \ topology
    /\ topology' = topology \cup {table}
    /\ faultLedger' = faultLedger \cup {table}
    /\ UNCHANGED <<phase, baseLeaves, leaves, leafLog, ledger,
                    mutationPublished, flushCount, leafFramesFreed,
                    tableFramesFreed, failureReturned>>

PublishLeaf(leaf) ==
    /\ phase \in {"prepared", "publishing"}
    /\ leaf \in Leaves \ leaves
    /\ topology # {}
    /\ phase' = "publishing"
    /\ leaves' = leaves \cup {leaf}
    /\ leafLog' = Append(leafLog, leaf)
    /\ mutationPublished' = TRUE
    /\ UNCHANGED <<baseLeaves, topology, ledger, faultLedger, flushCount,
                    leafFramesFreed, tableFramesFreed, failureReturned>>

BeginRollback ==
    /\ phase \in {"prepared", "publishing"}
    /\ phase' = "rolling-back"
    /\ UNCHANGED <<baseLeaves, leaves, leafLog, topology, ledger, faultLedger,
                    mutationPublished, flushCount, leafFramesFreed,
                    tableFramesFreed, failureReturned>>

RollbackLast ==
    /\ phase = "rolling-back"
    /\ Len(leafLog) > 0
    /\ LET last == leafLog[Len(leafLog)] IN
       /\ leaves' = leaves \ {last}
       /\ leafLog' = SubSeq(leafLog, 1, Len(leafLog) - 1)
    /\ UNCHANGED <<phase, baseLeaves, topology, ledger, faultLedger,
                    mutationPublished, flushCount, leafFramesFreed,
                    tableFramesFreed, failureReturned>>

FinishRollback ==
    /\ phase = "rolling-back"
    /\ Len(leafLog) = 0
    /\ leaves = baseLeaves
    /\ phase' = "failed"
    /\ flushCount' = IF mutationPublished THEN 1 ELSE 0
    /\ leafFramesFreed' = TRUE
    /\ failureReturned' = TRUE
    /\ UNCHANGED <<baseLeaves, leaves, leafLog, topology, ledger, faultLedger,
                    mutationPublished, tableFramesFreed>>

Commit ==
    /\ phase \in {"prepared", "publishing"}
    /\ phase' = "committed"
    /\ leafLog' = <<>>
    /\ UNCHANGED <<baseLeaves, leaves, topology, ledger, faultLedger,
                    mutationPublished, flushCount, leafFramesFreed,
                    tableFramesFreed, failureReturned>>

(* Retirement is the only step that reclaims a table frame, and it reconciles
   the two ledgers before it does. *)
Retire ==
    /\ phase \in {"failed", "committed"}
    /\ phase' = "retired"
    /\ tableFramesFreed' = TRUE
    /\ UNCHANGED <<baseLeaves, leaves, leafLog, topology, ledger, faultLedger,
                    mutationPublished, flushCount, leafFramesFreed,
                    failureReturned>>

Terminal ==
    /\ phase = "retired"
    /\ UNCHANGED vars

Next == Prepare \/ BeginRollback \/ RollbackLast \/ FinishRollback \/ Commit
        \/ Retire \/ Terminal
        \/ \E installed \in Tables: InstallTable(installed)
        \/ \E faulted \in Tables: FaultInstallTable(faulted)
        \/ \E leaf \in Leaves: PublishLeaf(leaf)

Spec == Init /\ [][Next]_vars /\ WF_vars(RollbackLast) /\ WF_vars(FinishRollback)

TypeOK ==
    /\ phase \in {"idle", "prepared", "publishing", "rolling-back",
                   "failed", "committed", "retired"}
    /\ baseLeaves \subseteq Leaves
    /\ leaves \subseteq Leaves
    /\ \A index \in 1..Len(leafLog): leafLog[index] \in Leaves
    /\ topology \subseteq Tables
    /\ ledger \subseteq Tables
    /\ faultLedger \subseteq Tables
    /\ mutationPublished \in BOOLEAN
    /\ flushCount \in 0..1
    /\ leafFramesFreed \in BOOLEAN
    /\ tableFramesFreed \in BOOLEAN
    /\ failureReturned \in BOOLEAN

(* A failed transaction owes the caller the leaf topology it started with. *)
FailureIsLeafTopologyNeutral == failureReturned => leaves = baseLeaves
FailureReturnsAfterRollback == failureReturned => Len(leafLog) = 0

(* ...and keeps the tables. Withdrawing one would race a concurrent installer
   that may already hold a leaf inside it. *)
FailureRetainsPublishedTables ==
    failureReturned => \A table \in ledger: table \in topology

(* The retirement cross-check: one frame, one ledger. *)
LedgersNeverClaimTheSameTable == ledger \cap faultLedger = {}
EveryReachableTableIsOwned == topology = ledger \cup faultLedger

ReclaimFollowsSingleFlush ==
    leafFramesFreed /\ mutationPublished => flushCount = 1
OneTransactionOneFlush == flushCount <= 1

(* No table frame returns to the allocator while the address space can still
   be walked; only retirement frees one. *)
NoTableReclaimBeforeRetirement ==
    phase # "retired" => ~tableFramesFreed

PublishedLeavesHaveRollbackCustody ==
    phase \in {"publishing", "rolling-back"} =>
        leaves \ baseLeaves = {leafLog[index] : index \in 1..Len(leafLog)}

RollbackEventuallyTerminates == phase = "rolling-back" ~> phase = "failed"

=============================================================================
