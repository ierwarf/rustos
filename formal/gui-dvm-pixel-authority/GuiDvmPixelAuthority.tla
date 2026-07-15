----------------------- MODULE GuiDvmPixelAuthority -----------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Security and completeness model for the split GUI-DVM transport.

The ivshmem BAR is an uncached control plane.  The bulk pixel pool is a
separate cacheable memory device: RustOS maps it writable, while QEMU exposes
it read-only to the Linux DVM.  Every READY slot is a complete immutable frame
snapshot even when its PRESENT record carries a smaller damage hint.  The DVM
may select and read that snapshot, but a write attempt is rejected without
changing any pixel epoch.  Loss of either plane revokes the transport.
*******************************************************************************)

CONSTANTS Slots, MaxGeneration, MaxRejectedWrites

Free == "free"
Writing == "writing"
Ready == "ready"
SlotStates == {Free, Writing, Ready}
NoSlot == MaxGeneration + 1
Host == "host"
NoWriter == "none"

VARIABLES slotState,
          slotGeneration,
          slotComplete,
          pixelEpoch,
          lastPixelWriter,
          selectedSlot,
          selectedGeneration,
          displayedGeneration,
          publishedGeneration,
          controlOnline,
          pixelOnline,
          dvmReadOnly,
          transportRevoked,
          rejectedDvmWrites

vars == <<slotState, slotGeneration, slotComplete, pixelEpoch,
          lastPixelWriter, selectedSlot, selectedGeneration,
          displayedGeneration, publishedGeneration, controlOnline,
          pixelOnline, dvmReadOnly, transportRevoked, rejectedDvmWrites>>

ReadySlots == {s \in Slots : slotState[s] = Ready}

Init ==
    /\ slotState = [s \in Slots |-> Free]
    /\ slotGeneration = [s \in Slots |-> 0]
    /\ slotComplete = [s \in Slots |-> FALSE]
    /\ pixelEpoch = [s \in Slots |-> 0]
    /\ lastPixelWriter = [s \in Slots |-> NoWriter]
    /\ selectedSlot = NoSlot
    /\ selectedGeneration = 0
    /\ displayedGeneration = 0
    /\ publishedGeneration = 0
    /\ controlOnline = TRUE
    /\ pixelOnline = TRUE
    /\ dvmReadOnly = TRUE
    /\ transportRevoked = FALSE
    /\ rejectedDvmWrites = 0

HostBegin(s) ==
    /\ controlOnline /\ pixelOnline /\ ~transportRevoked
    /\ slotState[s] = Free
    /\ publishedGeneration < MaxGeneration
    /\ slotState' = [slotState EXCEPT ![s] = Writing]
    /\ slotComplete' = [slotComplete EXCEPT ![s] = FALSE]
    /\ lastPixelWriter' = [lastPixelWriter EXCEPT ![s] = Host]
    /\ UNCHANGED <<slotGeneration, pixelEpoch, selectedSlot,
                  selectedGeneration, displayedGeneration,
                  publishedGeneration, controlOnline, pixelOnline,
                  dvmReadOnly, transportRevoked, rejectedDvmWrites>>

HostFinishSnapshot(s) ==
    /\ controlOnline /\ pixelOnline /\ ~transportRevoked
    /\ slotState[s] = Writing
    /\ ~slotComplete[s]
    /\ pixelEpoch[s] < MaxGeneration
    /\ slotComplete' = [slotComplete EXCEPT ![s] = TRUE]
    /\ pixelEpoch' = [pixelEpoch EXCEPT ![s] = pixelEpoch[s] + 1]
    /\ UNCHANGED <<slotState, slotGeneration, lastPixelWriter,
                  selectedSlot, selectedGeneration, displayedGeneration,
                  publishedGeneration, controlOnline, pixelOnline,
                  dvmReadOnly, transportRevoked, rejectedDvmWrites>>

HostPublish(s) ==
    /\ controlOnline /\ pixelOnline /\ ~transportRevoked
    /\ slotState[s] = Writing
    /\ slotComplete[s]
    /\ lastPixelWriter[s] = Host
    /\ publishedGeneration < MaxGeneration
    /\ slotState' = [slotState EXCEPT ![s] = Ready]
    /\ slotGeneration' = [slotGeneration EXCEPT ![s] = publishedGeneration + 1]
    /\ publishedGeneration' = publishedGeneration + 1
    /\ UNCHANGED <<slotComplete, pixelEpoch, lastPixelWriter,
                  selectedSlot, selectedGeneration, displayedGeneration,
                  controlOnline, pixelOnline, dvmReadOnly,
                  transportRevoked, rejectedDvmWrites>>

DvmSelect(s) ==
    /\ controlOnline /\ pixelOnline /\ dvmReadOnly /\ ~transportRevoked
    /\ selectedSlot = NoSlot
    /\ slotState[s] = Ready
    /\ slotComplete[s]
    /\ slotGeneration[s] > displayedGeneration
    /\ \A t \in ReadySlots : slotGeneration[t] <= slotGeneration[s]
    /\ selectedSlot' = s
    /\ selectedGeneration' = slotGeneration[s]
    /\ UNCHANGED <<slotState, slotGeneration, slotComplete, pixelEpoch,
                  lastPixelWriter, displayedGeneration, publishedGeneration,
                  controlOnline, pixelOnline, dvmReadOnly,
                  transportRevoked, rejectedDvmWrites>>

DvmDisplayAndRelease ==
    /\ selectedSlot \in Slots
    /\ controlOnline /\ pixelOnline /\ dvmReadOnly /\ ~transportRevoked
    /\ slotState[selectedSlot] = Ready
    /\ slotComplete[selectedSlot]
    /\ slotGeneration[selectedSlot] = selectedGeneration
    /\ slotState' = [slotState EXCEPT ![selectedSlot] = Free]
    /\ displayedGeneration' = selectedGeneration
    /\ selectedSlot' = NoSlot
    /\ selectedGeneration' = 0
    /\ UNCHANGED <<slotGeneration, slotComplete, pixelEpoch,
                  lastPixelWriter, publishedGeneration, controlOnline,
                  pixelOnline, dvmReadOnly, transportRevoked,
                  rejectedDvmWrites>>

DvmWriteAttempt ==
    /\ pixelOnline /\ dvmReadOnly
    /\ rejectedDvmWrites < MaxRejectedWrites
    /\ rejectedDvmWrites' = rejectedDvmWrites + 1
    /\ UNCHANGED <<slotState, slotGeneration, slotComplete, pixelEpoch,
                  lastPixelWriter, selectedSlot, selectedGeneration,
                  displayedGeneration, publishedGeneration, controlOnline,
                  pixelOnline, dvmReadOnly, transportRevoked>>

ControlPlaneLost ==
    /\ controlOnline /\ ~transportRevoked
    /\ controlOnline' = FALSE
    /\ transportRevoked' = TRUE
    /\ selectedSlot' = NoSlot
    /\ selectedGeneration' = 0
    /\ UNCHANGED <<slotState, slotGeneration, slotComplete, pixelEpoch,
                  lastPixelWriter, displayedGeneration, publishedGeneration,
                  pixelOnline, dvmReadOnly, rejectedDvmWrites>>

PixelPlaneLost ==
    /\ pixelOnline /\ ~transportRevoked
    /\ pixelOnline' = FALSE
    /\ transportRevoked' = TRUE
    /\ selectedSlot' = NoSlot
    /\ selectedGeneration' = 0
    /\ UNCHANGED <<slotState, slotGeneration, slotComplete, pixelEpoch,
                  lastPixelWriter, displayedGeneration, publishedGeneration,
                  controlOnline, dvmReadOnly, rejectedDvmWrites>>

ForgedControl ==
    /\ ~transportRevoked
    /\ transportRevoked' = TRUE
    /\ selectedSlot' = NoSlot
    /\ selectedGeneration' = 0
    /\ UNCHANGED <<slotState, slotGeneration, slotComplete, pixelEpoch,
                  lastPixelWriter, displayedGeneration, publishedGeneration,
                  controlOnline, pixelOnline, dvmReadOnly, rejectedDvmWrites>>

Next ==
    \/ \E s \in Slots : HostBegin(s)
    \/ \E s \in Slots : HostFinishSnapshot(s)
    \/ \E s \in Slots : HostPublish(s)
    \/ \E s \in Slots : DvmSelect(s)
    \/ DvmDisplayAndRelease
    \/ DvmWriteAttempt
    \/ ControlPlaneLost
    \/ PixelPlaneLost
    \/ ForgedControl

TypeOK ==
    /\ slotState \in [Slots -> SlotStates]
    /\ slotGeneration \in [Slots -> 0..MaxGeneration]
    /\ slotComplete \in [Slots -> BOOLEAN]
    /\ pixelEpoch \in [Slots -> 0..MaxGeneration]
    /\ lastPixelWriter \in [Slots -> {NoWriter, Host}]
    /\ selectedSlot \in Slots \cup {NoSlot}
    /\ selectedGeneration \in 0..MaxGeneration
    /\ displayedGeneration \in 0..MaxGeneration
    /\ publishedGeneration \in 0..MaxGeneration
    /\ controlOnline \in BOOLEAN
    /\ pixelOnline \in BOOLEAN
    /\ dvmReadOnly \in BOOLEAN
    /\ transportRevoked \in BOOLEAN
    /\ rejectedDvmWrites \in 0..MaxRejectedWrites

ReadySlotsAreCompleteHostSnapshots ==
    \A s \in ReadySlots :
        /\ slotComplete[s]
        /\ lastPixelWriter[s] = Host
        /\ slotGeneration[s] > 0

DvmHasNoPixelWriteAuthority ==
    /\ dvmReadOnly
    /\ \A s \in Slots : lastPixelWriter[s] # "dvm"

SelectionIsFreshCompleteAndReadOnly ==
    selectedSlot \in Slots =>
        /\ controlOnline /\ pixelOnline /\ ~transportRevoked /\ dvmReadOnly
        /\ slotState[selectedSlot] = Ready
        /\ slotComplete[selectedSlot]
        /\ slotGeneration[selectedSlot] = selectedGeneration
        /\ selectedGeneration > displayedGeneration

PlaneLossFailsClosed ==
    (~controlOnline \/ ~pixelOnline) => transportRevoked

RevocationClearsSelection ==
    transportRevoked => selectedSlot = NoSlot /\ selectedGeneration = 0

DisplayedGenerationNeverExceedsPublished ==
    displayedGeneration <= publishedGeneration

Spec == Init /\ [][Next]_vars
=============================================================================
