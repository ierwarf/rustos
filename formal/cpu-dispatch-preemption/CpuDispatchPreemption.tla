------------------------- MODULE CpuDispatchPreemption -------------------------
EXTENDS Naturals
\* Original own-CPU non-lock IRQ/preemption scope, raw directory and exact
\* priority ordering through Current/Transition publication and assembly RSP.
\* Choose is the original turn's non-Copy Choice; a foreign masked publisher
\* owns PriorityPublish only outside its raw leaves (actor scope is earlier
\* modeled input). Raw-directory release is NOT original CPU scope release.
\* No MM close, frame/funding, register hardware or full joint reuse claim.
VARIABLES phase, owned, location, irqEnabled, savedIF, rawPending, rawHeld,
          changed, incomingRsp, releasedBeforeRsp, priority, pairPublished,
          choiceReleasedWithoutPair
vars == <<phase, owned, location, irqEnabled, savedIF, rawPending, rawHeld,
          changed, incomingRsp, releasedBeforeRsp, priority, pairPublished,
          choiceReleasedWithoutPair>>
Init == /\ phase = "Idle" /\ owned = FALSE /\ location = "None"
        /\ irqEnabled \in BOOLEAN /\ savedIF = FALSE
        /\ rawPending = FALSE /\ rawHeld = FALSE /\ changed = FALSE
        /\ incomingRsp = FALSE /\ releasedBeforeRsp = FALSE
        /\ priority = "Vacant" /\ pairPublished = FALSE
        /\ choiceReleasedWithoutPair = FALSE
Enter ==
    /\ phase = "Idle" /\ ~owned
    /\ owned' = TRUE /\ location' = "Rust" /\ phase' = "Open"
    /\ savedIF' = irqEnabled /\ UNCHANGED irqEnabled
    /\ incomingRsp' = FALSE /\ releasedBeforeRsp' = FALSE
    /\ pairPublished' = FALSE
    /\ UNCHANGED <<rawPending, rawHeld, changed, priority, choiceReleasedWithoutPair>>
ReserveRaw ==
    /\ phase = "Open" /\ owned /\ ~rawPending /\ ~rawHeld /\ priority # "Choosing"
    /\ rawPending' = TRUE
    /\ UNCHANGED <<phase, owned, location, irqEnabled, savedIF, rawHeld,
                    changed, incomingRsp, releasedBeforeRsp, priority,
                    pairPublished, choiceReleasedWithoutPair>>
AcquireRaw ==
    /\ phase = "Open" /\ owned /\ rawPending
    /\ rawHeld' = TRUE /\ rawPending' = FALSE
    /\ UNCHANGED <<phase, owned, location, irqEnabled, savedIF,
                    changed, incomingRsp, releasedBeforeRsp, priority,
                    pairPublished, choiceReleasedWithoutPair>>
MaskProtected ==
    /\ phase = "Open" /\ owned /\ irqEnabled
    /\ irqEnabled' = FALSE
    /\ UNCHANGED <<phase, owned, location, savedIF, rawPending, rawHeld,
                    changed, incomingRsp, releasedBeforeRsp, priority,
                    pairPublished, choiceReleasedWithoutPair>>
\* Actual core drops its raw directory BEFORE choosing/validating/claiming.
UnlockForChoice ==
    /\ phase = "Open" /\ owned /\ ~irqEnabled /\ rawHeld /\ ~rawPending
    /\ rawHeld' = FALSE
    /\ UNCHANGED <<phase, owned, location, irqEnabled, savedIF, rawPending,
                    changed, incomingRsp, releasedBeforeRsp, priority,
                    pairPublished, choiceReleasedWithoutPair>>
Choose ==
    /\ phase = "Open" /\ owned /\ ~irqEnabled /\ ~rawPending /\ ~rawHeld
    /\ priority = "Vacant" /\ priority' = "Choosing"
    /\ UNCHANGED <<phase, owned, location, irqEnabled, savedIF, rawPending, rawHeld,
                    changed, incomingRsp, releasedBeforeRsp, pairPublished,
                    choiceReleasedWithoutPair>>
\* A competing publisher waits for the ORIGINAL choice, not a scalar sample
\* of an empty FIFO or a completed owner generation. Its actor retains loans.
PriorityPublish ==
    /\ priority = "Vacant" /\ phase # "Poisoned"
    /\ priority' = "Publishing"
    /\ UNCHANGED <<phase, owned, location, irqEnabled, savedIF, rawPending, rawHeld,
                    changed, incomingRsp, releasedBeforeRsp, pairPublished,
                    choiceReleasedWithoutPair>>
PriorityPublicationDone ==
    /\ priority = "Publishing" /\ priority' = "Vacant"
    /\ UNCHANGED <<phase, owned, location, irqEnabled, savedIF, rawPending, rawHeld,
                    changed, incomingRsp, releasedBeforeRsp, pairPublished,
                    choiceReleasedWithoutPair>>
Publish(c) ==
    /\ phase = "Open" /\ owned /\ ~irqEnabled /\ ~rawPending
    /\ ((rawHeld /\ priority # "Choosing") \/ (~rawHeld /\ priority = "Choosing"))
    /\ phase' = "Publishing" /\ changed' = c /\ pairPublished' = TRUE
    /\ UNCHANGED <<owned, location, irqEnabled, savedIF, rawPending, rawHeld,
                    incomingRsp, releasedBeforeRsp, priority, choiceReleasedWithoutPair>>
ReleaseChoice ==
    /\ phase = "Publishing" /\ owned /\ priority = "Choosing" /\ pairPublished
    /\ priority' = "Vacant" /\ choiceReleasedWithoutPair' = ~pairPublished
    /\ UNCHANGED <<phase, owned, location, irqEnabled, savedIF, rawPending, rawHeld,
                    changed, incomingRsp, releasedBeforeRsp, pairPublished>>
Unlock ==
    /\ phase = "Publishing" /\ rawHeld /\ rawHeld' = FALSE
    /\ UNCHANGED <<phase, owned, location, irqEnabled, savedIF, rawPending,
                    changed, incomingRsp, releasedBeforeRsp, priority,
                    pairPublished, choiceReleasedWithoutPair>>
CompleteChanged ==
    /\ phase = "Publishing" /\ changed /\ ~rawHeld /\ ~rawPending
    /\ priority # "Choosing" /\ pairPublished
    /\ phase' = "AwaitingRsp" /\ location' = "Parked" /\ savedIF' = FALSE
    /\ UNCHANGED <<owned, irqEnabled, rawPending, rawHeld, changed,
                    incomingRsp, releasedBeforeRsp, priority, pairPublished,
                    choiceReleasedWithoutPair>>
CompleteSame ==
    /\ phase = "Publishing" /\ ~changed /\ ~rawHeld /\ ~rawPending
    /\ priority # "Choosing" /\ pairPublished
    /\ phase' = "Idle" /\ location' = "None" /\ owned' = FALSE
    /\ irqEnabled' = savedIF
    /\ UNCHANGED <<savedIF, rawPending, rawHeld, changed, incomingRsp, releasedBeforeRsp,
                    priority, pairPublished, choiceReleasedWithoutPair>>
AckRsp ==
    /\ phase = "AwaitingRsp" /\ phase' = "Committing" /\ incomingRsp' = TRUE
    /\ UNCHANGED <<owned, location, irqEnabled, savedIF, rawPending, rawHeld,
                    changed, releasedBeforeRsp, priority, pairPublished,
                    choiceReleasedWithoutPair>>
ReleaseParked ==
    /\ phase = "Committing" /\ owned /\ location = "Parked"
    /\ owned' = FALSE /\ location' = "None" /\ irqEnabled' = savedIF
    /\ releasedBeforeRsp' = ~incomingRsp
    /\ UNCHANGED <<phase, savedIF, rawPending, rawHeld, changed, incomingRsp,
                    priority, pairPublished, choiceReleasedWithoutPair>>
Reopen ==
    /\ phase = "Committing" /\ ~owned /\ phase' = "Idle"
    /\ UNCHANGED <<owned, location, irqEnabled, savedIF, rawPending, rawHeld,
                    changed, incomingRsp, releasedBeforeRsp, priority,
                    pairPublished, choiceReleasedWithoutPair>>
\* Panic poisons an original Choice, NEVER acknowledging Current or RSP. The
\* independent publisher may complete its own prior publication, not recovery.
Poison ==
    /\ phase \in {"Open", "Publishing"}
    /\ phase' = "Poisoned" /\ rawPending' = FALSE /\ rawHeld' = FALSE
    /\ owned' = FALSE /\ location' = "None" /\ irqEnabled' = FALSE
    /\ priority' = IF priority = "Choosing" THEN "Poisoned" ELSE priority
    /\ UNCHANGED <<savedIF, changed, incomingRsp, releasedBeforeRsp,
                    pairPublished, choiceReleasedWithoutPair>>
Stutter == /\ phase \in {"Idle", "Poisoned", "Open"} /\ UNCHANGED vars
Next == Enter \/ ReserveRaw \/ AcquireRaw \/ MaskProtected \/ UnlockForChoice \/ Choose
        \/ PriorityPublish \/ PriorityPublicationDone \/ (\E c \in BOOLEAN: Publish(c))
        \/ ReleaseChoice \/ Unlock \/ CompleteChanged \/ CompleteSame \/ AckRsp
        \/ ReleaseParked \/ Reopen \/ Poison \/ Stutter
TypeOK == /\ phase \in {"Idle", "Open", "Publishing", "AwaitingRsp", "Committing", "Poisoned"}
          /\ location \in {"None", "Rust", "Parked"}
          /\ owned \in BOOLEAN /\ irqEnabled \in BOOLEAN /\ savedIF \in BOOLEAN
          /\ rawPending \in BOOLEAN /\ rawHeld \in BOOLEAN /\ changed \in BOOLEAN
          /\ incomingRsp \in BOOLEAN /\ releasedBeforeRsp \in BOOLEAN
          /\ priority \in {"Vacant", "Choosing", "Publishing", "Poisoned"}
          /\ pairPublished \in BOOLEAN /\ choiceReleasedWithoutPair \in BOOLEAN
ProducerAndAwaitRetainScope == phase \in {"Open", "Publishing", "AwaitingRsp"} => owned
NoIrqAcrossProtectedDispatchOrCommit == phase \in {"Publishing", "AwaitingRsp", "Committing"} => ~irqEnabled
NoReleaseBeforeIncomingRsp == ~releasedBeforeRsp
ParkedHasExactConsumer == (location = "Parked") => phase \in {"AwaitingRsp", "Committing"}
DirectoryCannotOutliveScope == (rawHeld \/ rawPending) => owned
IdleAndPoisonCannotOwnScope == phase \in {"Idle", "Poisoned"} => ~owned
OriginalChoiceRetainsScope == priority = "Choosing" =>
    owned /\ ~irqEnabled /\ ~rawHeld /\ ~rawPending /\ phase \in {"Open", "Publishing"}
NoChoiceReleaseBeforePair == ~choiceReleasedWithoutPair
PublicationAndReturnAreDistinct == phase \in {"AwaitingRsp", "Committing"} =>
    pairPublished /\ priority # "Choosing"
Spec == Init /\ [][Next]_vars
===============================================================================
