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

CONSTANTS Slots, Sources, MaxGeneration, MaxRejectedWrites

Free == "free"
Writing == "writing"
Ready == "ready"
SlotStates == {Free, Writing, Ready}
NoSlot == MaxGeneration + 1
Host == "host"
NoWriter == "none"
NoSource == "none"
NoCopy == "none"
FullCopy == "full"
DamageCopy == "damage"
CopyModes == {NoCopy, FullCopy, DamageCopy}

VARIABLES slotState,
          slotGeneration,
          slotContentGeneration,
          slotComplete,
          pixelEpoch,
          lastPixelWriter,
          copyMode,
          copyBaseGeneration,
          copySource,
          activeSource,
          publishedSource,
          selectedSlot,
          selectedGeneration,
          displayedGeneration,
          publishedGeneration,
          controlOnline,
          pixelOnline,
          dvmReadOnly,
          transportRevoked,
          rejectedDvmWrites

vars == <<slotState, slotGeneration, slotContentGeneration, slotComplete, pixelEpoch,
          lastPixelWriter, copyMode, copyBaseGeneration, copySource,
          activeSource, publishedSource, selectedSlot, selectedGeneration,
          displayedGeneration, publishedGeneration, controlOnline,
          pixelOnline, dvmReadOnly, transportRevoked, rejectedDvmWrites>>

ReadySlots == {s \in Slots : slotState[s] = Ready}

Init ==
    /\ slotState = [s \in Slots |-> Free]
    /\ slotGeneration = [s \in Slots |-> 0]
    /\ slotContentGeneration = [s \in Slots |-> 0]
    /\ slotComplete = [s \in Slots |-> FALSE]
    /\ pixelEpoch = [s \in Slots |-> 0]
    /\ lastPixelWriter = [s \in Slots |-> NoWriter]
    /\ copyMode = [s \in Slots |-> NoCopy]
    /\ copyBaseGeneration = [s \in Slots |-> 0]
    /\ copySource = [s \in Slots |-> NoSource]
    /\ activeSource \in Sources
    /\ publishedSource = NoSource
    /\ selectedSlot = NoSlot
    /\ selectedGeneration = 0
    /\ displayedGeneration = 0
    /\ publishedGeneration = 0
    /\ controlOnline = TRUE
    /\ pixelOnline = TRUE
    /\ dvmReadOnly = TRUE
    /\ transportRevoked = FALSE
    /\ rejectedDvmWrites = 0

NoHostWriter == \A t \in Slots : slotState[t] # Writing

HostBeginFull(s) ==
    /\ controlOnline /\ pixelOnline /\ ~transportRevoked
    /\ slotState[s] = Free
    /\ NoHostWriter
    /\ publishedGeneration < MaxGeneration
    /\ slotState' = [slotState EXCEPT ![s] = Writing]
    /\ slotComplete' = [slotComplete EXCEPT ![s] = FALSE]
    /\ lastPixelWriter' = [lastPixelWriter EXCEPT ![s] = Host]
    /\ copyMode' = [copyMode EXCEPT ![s] = FullCopy]
    /\ copyBaseGeneration' = [copyBaseGeneration EXCEPT ![s] = slotContentGeneration[s]]
    /\ copySource' = [copySource EXCEPT ![s] = activeSource]
    /\ UNCHANGED <<slotGeneration, slotContentGeneration, pixelEpoch,
                  activeSource, publishedSource, selectedSlot,
                  selectedGeneration, displayedGeneration, publishedGeneration,
                  controlOnline, pixelOnline, dvmReadOnly, transportRevoked,
                  rejectedDvmWrites>>

HostBeginDamage(s) ==
    /\ controlOnline /\ pixelOnline /\ ~transportRevoked
    /\ slotState[s] = Free
    /\ NoHostWriter
    /\ publishedGeneration > 0
    /\ publishedGeneration < MaxGeneration
    /\ slotContentGeneration[s] = publishedGeneration
    /\ activeSource = publishedSource
    /\ slotState' = [slotState EXCEPT ![s] = Writing]
    /\ slotComplete' = [slotComplete EXCEPT ![s] = FALSE]
    /\ lastPixelWriter' = [lastPixelWriter EXCEPT ![s] = Host]
    /\ copyMode' = [copyMode EXCEPT ![s] = DamageCopy]
    /\ copyBaseGeneration' = [copyBaseGeneration EXCEPT ![s] = publishedGeneration]
    /\ copySource' = [copySource EXCEPT ![s] = activeSource]
    /\ UNCHANGED <<slotGeneration, slotContentGeneration, pixelEpoch,
                  activeSource, publishedSource, selectedSlot,
                  selectedGeneration, displayedGeneration, publishedGeneration,
                  controlOnline, pixelOnline, dvmReadOnly, transportRevoked,
                  rejectedDvmWrites>>

HostFinishSnapshot(s) ==
    /\ controlOnline /\ pixelOnline /\ ~transportRevoked
    /\ slotState[s] = Writing
    /\ ~slotComplete[s]
    /\ pixelEpoch[s] < MaxGeneration
    /\ slotComplete' = [slotComplete EXCEPT ![s] = TRUE]
    /\ pixelEpoch' = [pixelEpoch EXCEPT ![s] = pixelEpoch[s] + 1]
    /\ slotContentGeneration' =
           [slotContentGeneration EXCEPT ![s] = publishedGeneration + 1]
    /\ UNCHANGED <<slotState, slotGeneration, lastPixelWriter,
                  copyMode, copyBaseGeneration, copySource, activeSource,
                  publishedSource, selectedSlot, selectedGeneration,
                  displayedGeneration, publishedGeneration, controlOnline,
                  pixelOnline, dvmReadOnly, transportRevoked, rejectedDvmWrites>>

HostPublish(s) ==
    /\ controlOnline /\ pixelOnline /\ ~transportRevoked
    /\ slotState[s] = Writing
    /\ slotComplete[s]
    /\ lastPixelWriter[s] = Host
    /\ copyMode[s] \in {FullCopy, DamageCopy}
    /\ copySource[s] \in Sources
    /\ publishedGeneration < MaxGeneration
    /\ slotContentGeneration[s] = publishedGeneration + 1
    /\ slotState' = [slotState EXCEPT ![s] = Ready]
    /\ slotGeneration' = [slotGeneration EXCEPT ![s] = publishedGeneration + 1]
    /\ publishedGeneration' = publishedGeneration + 1
    /\ publishedSource' = copySource[s]
    /\ copyMode' = [copyMode EXCEPT ![s] = NoCopy]
    /\ copyBaseGeneration' = [copyBaseGeneration EXCEPT ![s] = 0]
    /\ copySource' = [copySource EXCEPT ![s] = NoSource]
    /\ UNCHANGED <<slotContentGeneration, slotComplete, pixelEpoch,
                  lastPixelWriter, activeSource, selectedSlot,
                  selectedGeneration, displayedGeneration, controlOnline,
                  pixelOnline, dvmReadOnly, transportRevoked,
                  rejectedDvmWrites>>

SwitchSource(source) ==
    /\ source \in Sources
    /\ source # activeSource
    /\ NoHostWriter
    /\ activeSource' = source
    /\ UNCHANGED <<slotState, slotGeneration, slotContentGeneration,
                  slotComplete, pixelEpoch, lastPixelWriter, copyMode,
                  copyBaseGeneration, copySource, publishedSource,
                  selectedSlot, selectedGeneration, displayedGeneration,
                  publishedGeneration, controlOnline, pixelOnline,
                  dvmReadOnly, transportRevoked, rejectedDvmWrites>>

DvmSelect(s) ==
    /\ controlOnline /\ pixelOnline /\ dvmReadOnly /\ ~transportRevoked
    /\ selectedSlot = NoSlot
    /\ slotState[s] = Ready
    /\ slotComplete[s]
    /\ slotGeneration[s] > displayedGeneration
    /\ \A t \in ReadySlots : slotGeneration[t] <= slotGeneration[s]
    /\ selectedSlot' = s
    /\ selectedGeneration' = slotGeneration[s]
    /\ UNCHANGED <<slotState, slotGeneration, slotContentGeneration,
                  slotComplete, pixelEpoch, lastPixelWriter, copyMode,
                  copyBaseGeneration, copySource, activeSource,
                  publishedSource, displayedGeneration, publishedGeneration,
                  controlOnline, pixelOnline, dvmReadOnly, transportRevoked,
                  rejectedDvmWrites>>

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
    /\ UNCHANGED <<slotGeneration, slotContentGeneration, slotComplete,
                  pixelEpoch, lastPixelWriter, copyMode, copyBaseGeneration,
                  copySource, activeSource, publishedSource,
                  publishedGeneration, controlOnline, pixelOnline,
                  dvmReadOnly, transportRevoked, rejectedDvmWrites>>

DvmWriteAttempt ==
    /\ pixelOnline /\ dvmReadOnly
    /\ rejectedDvmWrites < MaxRejectedWrites
    /\ rejectedDvmWrites' = rejectedDvmWrites + 1
    /\ UNCHANGED <<slotState, slotGeneration, slotContentGeneration,
                  slotComplete, pixelEpoch, lastPixelWriter, copyMode,
                  copyBaseGeneration, copySource, activeSource,
                  publishedSource, selectedSlot, selectedGeneration,
                  displayedGeneration, publishedGeneration, controlOnline,
                  pixelOnline, dvmReadOnly, transportRevoked>>

ControlPlaneLost ==
    /\ controlOnline /\ ~transportRevoked
    /\ controlOnline' = FALSE
    /\ transportRevoked' = TRUE
    /\ selectedSlot' = NoSlot
    /\ selectedGeneration' = 0
    /\ UNCHANGED <<slotState, slotGeneration, slotContentGeneration,
                  slotComplete, pixelEpoch, lastPixelWriter, copyMode,
                  copyBaseGeneration, copySource, activeSource,
                  publishedSource, displayedGeneration, publishedGeneration,
                  pixelOnline, dvmReadOnly, rejectedDvmWrites>>

PixelPlaneLost ==
    /\ pixelOnline /\ ~transportRevoked
    /\ pixelOnline' = FALSE
    /\ transportRevoked' = TRUE
    /\ selectedSlot' = NoSlot
    /\ selectedGeneration' = 0
    /\ UNCHANGED <<slotState, slotGeneration, slotContentGeneration,
                  slotComplete, pixelEpoch, lastPixelWriter, copyMode,
                  copyBaseGeneration, copySource, activeSource,
                  publishedSource, displayedGeneration, publishedGeneration,
                  controlOnline, dvmReadOnly, rejectedDvmWrites>>

ForgedControl ==
    /\ ~transportRevoked
    /\ transportRevoked' = TRUE
    /\ selectedSlot' = NoSlot
    /\ selectedGeneration' = 0
    /\ UNCHANGED <<slotState, slotGeneration, slotContentGeneration,
                  slotComplete, pixelEpoch, lastPixelWriter, copyMode,
                  copyBaseGeneration, copySource, activeSource,
                  publishedSource, displayedGeneration, publishedGeneration,
                  controlOnline, pixelOnline, dvmReadOnly, rejectedDvmWrites>>

Next ==
    \/ \E s \in Slots : HostBeginFull(s)
    \/ \E s \in Slots : HostBeginDamage(s)
    \/ \E s \in Slots : HostFinishSnapshot(s)
    \/ \E s \in Slots : HostPublish(s)
    \/ \E s \in Slots : DvmSelect(s)
    \/ \E source \in Sources : SwitchSource(source)
    \/ DvmDisplayAndRelease
    \/ DvmWriteAttempt
    \/ ControlPlaneLost
    \/ PixelPlaneLost
    \/ ForgedControl

TypeOK ==
    /\ slotState \in [Slots -> SlotStates]
    /\ slotGeneration \in [Slots -> 0..MaxGeneration]
    /\ slotContentGeneration \in [Slots -> 0..MaxGeneration]
    /\ slotComplete \in [Slots -> BOOLEAN]
    /\ pixelEpoch \in [Slots -> 0..MaxGeneration]
    /\ lastPixelWriter \in [Slots -> {NoWriter, Host}]
    /\ copyMode \in [Slots -> CopyModes]
    /\ copyBaseGeneration \in [Slots -> 0..MaxGeneration]
    /\ copySource \in [Slots -> Sources \cup {NoSource}]
    /\ activeSource \in Sources
    /\ publishedSource \in Sources \cup {NoSource}
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
        /\ slotGeneration[s] = slotContentGeneration[s]

SingleHostWriter ==
    Cardinality({s \in Slots : slotState[s] = Writing}) <= 1

IdleSlotsHaveNoCopy ==
    \A s \in Slots :
        slotState[s] # Writing =>
            /\ copyMode[s] = NoCopy
            /\ copyBaseGeneration[s] = 0
            /\ copySource[s] = NoSource

DamageCopyHasExactPredecessor ==
    \A s \in Slots :
        copyMode[s] = DamageCopy =>
            /\ slotState[s] = Writing
            /\ copyBaseGeneration[s] = publishedGeneration
            /\ IF slotComplete[s]
                  THEN slotContentGeneration[s] = publishedGeneration + 1
                  ELSE slotContentGeneration[s] = publishedGeneration
            /\ publishedGeneration > 0
            /\ copySource[s] = publishedSource

WritingSlotHasCapturedSource ==
    \A s \in Slots :
        slotState[s] = Writing =>
            /\ copyMode[s] \in {FullCopy, DamageCopy}
            /\ copySource[s] \in Sources

PublishedSourceExists ==
    (publishedGeneration = 0) <=> (publishedSource = NoSource)

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
