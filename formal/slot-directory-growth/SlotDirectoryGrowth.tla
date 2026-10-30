-------------------- MODULE SlotDirectoryGrowth --------------------
EXTENDS Naturals

(***************************************************************************
Models growth of the process slot storage.

Concrete owners:
  * kernel/ps/src/multitask/slot_dir.rs  (AtomicDir::ensure / get,
        ChunkTable::build / push_chunk)
  * kernel/ps/src/multitask/process_table.rs  (grow_slots)
  * kernel/ps/src/multitask/process_table/spawn.rs  (reserve_spawn_in)

Every per-slot side array (identity, pins, placement, VMA tables, rewrite
epochs) is a directory of chunks that is published before the table chunk that
makes the same slot indexes allocatable. Chunks are only added, never moved or
freed, so a lock-free reader holding a reference never sees storage disappear.
***************************************************************************)

MaxChunks == 3
ChunkSize == 2
Slots == 0..(MaxChunks * ChunkSize - 1)

VARIABLES sideChunks, tableChunks, slot

vars == <<sideChunks, tableChunks, slot>>

TypeOK ==
    /\ sideChunks \in 0..MaxChunks
    /\ tableChunks \in 0..MaxChunks
    /\ slot \in [Slots -> {"free", "reserved"}]

Init ==
    /\ sideChunks = 0
    /\ tableChunks = 0
    /\ slot = [s \in Slots |-> "free"]

\* ensure_slot_storage for the next chunk of every side array.
GrowSide ==
    /\ sideChunks < MaxChunks
    /\ sideChunks' = sideChunks + 1
    /\ UNCHANGED <<tableChunks, slot>>

\* The table chunk is installed only after the side storage for it exists.
GrowTable ==
    /\ tableChunks < MaxChunks
    /\ tableChunks < sideChunks \* side storage first
    /\ tableChunks' = tableChunks + 1
    /\ UNCHANGED <<sideChunks, slot>>

Reserve(s) ==
    /\ s < tableChunks * ChunkSize
    /\ slot[s] = "free"
    /\ slot' = [slot EXCEPT ![s] = "reserved"]
    /\ UNCHANGED <<sideChunks, tableChunks>>

Release(s) ==
    /\ slot[s] = "reserved"
    /\ slot' = [slot EXCEPT ![s] = "free"]
    /\ UNCHANGED <<sideChunks, tableChunks>>

Next ==
    \/ GrowSide
    \/ GrowTable
    \/ \E s \in Slots: Reserve(s) \/ Release(s)
    \/ UNCHANGED vars \* the bounded storage is fully grown

Spec == Init /\ [][Next]_vars

\* The table never has a slot whose side storage is not yet published.
TableNeverAheadOfSide == tableChunks <= sideChunks

\* A reserved slot always has side storage behind its index.
ReservedSlotHasStorage ==
    \A s \in Slots: slot[s] = "reserved" => s < sideChunks * ChunkSize

\* Storage only grows: no action decreases a chunk count, so no chunk is ever
\* removed or replaced. This is structural (the only actions that touch the
\* counters add one); the safety profile checks state invariants only.

=============================================================================
