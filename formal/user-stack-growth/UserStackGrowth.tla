------------------------- MODULE UserStackGrowth -------------------------
EXTENDS Naturals

(***************************************************************************
The release containment eagerly maps every usable stack page and retains one
permanent guard, so a valid stack access never enters exception-time locking.
The deferred path remains the required future refinement for restoring lazy
growth: transient ProcessStateLock contention must defer, never retire, a live
valid fault.
***************************************************************************)

CONSTANT MaxGeneration
NoGeneration == MaxGeneration + 1

Idle == "idle"
Planned == "planned"
Applying == "applying"
Deferred == "deferred"
Mapped == "mapped"
Committed == "committed"
Retired == "retired"

VARIABLES phase, generation, planGeneration, schedulerRaw, processLock,
          mappingInstalled, metadataCommitted, taskRetired

vars == <<phase, generation, planGeneration, schedulerRaw, processLock,
          mappingInstalled, metadataCommitted, taskRetired>>

Init ==
    /\ phase = Idle
    /\ generation = 0
    /\ planGeneration = NoGeneration
    /\ schedulerRaw = FALSE
    /\ processLock = FALSE
    /\ mappingInstalled = FALSE
    /\ metadataCommitted = FALSE
    /\ taskRetired = FALSE

Prepare ==
    /\ phase = Idle /\ ~taskRetired
    /\ phase' = Planned
    /\ planGeneration' = generation
    /\ UNCHANGED <<generation, schedulerRaw, processLock, mappingInstalled,
                    metadataCommitted, taskRetired>>

EagerMapAllUsablePages ==
    /\ phase = Idle /\ ~taskRetired
    /\ phase' = Committed
    /\ planGeneration' = generation
    /\ mappingInstalled' = TRUE
    /\ metadataCommitted' = TRUE
    /\ UNCHANGED <<generation, schedulerRaw, processLock,
                    taskRetired>>

BeginApply ==
    /\ phase = Planned /\ ~taskRetired
    /\ phase' = Applying
    /\ processLock' = TRUE /\ schedulerRaw' = FALSE
    /\ UNCHANGED <<generation, planGeneration, mappingInstalled,
                    metadataCommitted, taskRetired>>

FinishApply ==
    /\ phase = Applying /\ processLock
    /\ phase' = Mapped
    /\ processLock' = FALSE
    /\ mappingInstalled' = TRUE
    /\ UNCHANGED <<generation, planGeneration, schedulerRaw,
                    metadataCommitted, taskRetired>>

ApplyContentionDefers ==
    /\ phase = Planned /\ ~taskRetired
    /\ phase' = Deferred
    /\ UNCHANGED <<generation, planGeneration, schedulerRaw, processLock,
                    mappingInstalled, metadataCommitted, taskRetired>>

RetryDeferred ==
    /\ phase = Deferred /\ ~taskRetired
    /\ phase' = Applying
    /\ processLock' = TRUE /\ schedulerRaw' = FALSE
    /\ UNCHANGED <<generation, planGeneration, mappingInstalled,
                    metadataCommitted, taskRetired>>

ConcurrentRetirement ==
    /\ phase \in {Planned, Mapped} /\ ~taskRetired
    /\ phase' = IF phase = Planned THEN Retired ELSE Mapped
    /\ taskRetired' = TRUE
    /\ UNCHANGED <<generation, planGeneration, schedulerRaw,
                    processLock, mappingInstalled, metadataCommitted>>

ConcurrentExec ==
    /\ phase \in {Planned, Deferred, Mapped} /\ ~processLock
    /\ generation < MaxGeneration
    /\ generation' = generation + 1
    /\ UNCHANGED <<phase, planGeneration, schedulerRaw, processLock,
                    mappingInstalled, metadataCommitted, taskRetired>>

CommitMetadata ==
    /\ phase = Mapped /\ mappingInstalled
    /\ ~taskRetired /\ generation = planGeneration
    /\ phase' = Committed /\ metadataCommitted' = TRUE
    /\ UNCHANGED <<generation, planGeneration, schedulerRaw, processLock,
                    mappingInstalled, taskRetired>>

RejectStaleCommit ==
    /\ phase = Mapped
    /\ taskRetired \/ generation # planGeneration
    \* The anonymous pages remain owned by the process address space and are
    \* still covered by its full stack VMA.  The faulting task is retired and
    \* process teardown reclaims the pages; reacquiring ProcessStateLock for a
    \* rollback would make exception recovery wait after losing its scheduler
    \* generation and would weaken the no-lock-overlap rule.
    /\ phase' = Retired /\ taskRetired' = TRUE
    /\ UNCHANGED <<generation, planGeneration, schedulerRaw, processLock,
                    mappingInstalled, metadataCommitted>>

Terminal ==
    /\ phase \in {Committed, Retired}
    /\ UNCHANGED vars

Next ==
    \/ EagerMapAllUsablePages
    \/ Prepare \/ BeginApply \/ FinishApply \/ ApplyContentionDefers
    \/ RetryDeferred
    \/ ConcurrentRetirement \/ ConcurrentExec
    \/ CommitMetadata \/ RejectStaleCommit \/ Terminal

Settle == BeginApply \/ FinishApply \/ ApplyContentionDefers \/ RetryDeferred
          \/ CommitMetadata \/ RejectStaleCommit

Spec == Init /\ [][Next]_vars /\ WF_vars(Settle)

TypeOK ==
    /\ phase \in {Idle, Planned, Applying, Deferred, Mapped, Committed, Retired}
    /\ generation \in 0..MaxGeneration
    /\ planGeneration \in 0..NoGeneration
    /\ schedulerRaw \in BOOLEAN /\ processLock \in BOOLEAN
    /\ mappingInstalled \in BOOLEAN /\ metadataCommitted \in BOOLEAN
    /\ taskRetired \in BOOLEAN

LockDomainsNeverOverlap == ~(schedulerRaw /\ processLock)
MetadataRequiresExactLiveMapping ==
    metadataCommitted => mappingInstalled /\ ~taskRetired
                         /\ generation = planGeneration
CommittedOrRetiredIsTerminal ==
    phase = Committed => metadataCommitted /\ ~taskRetired
RetainedRejectedMappingHasNoSchedulerCommit ==
    phase = Retired /\ mappingInstalled => ~metadataCommitted /\ taskRetired

TransientContentionCannotRetireValidFault ==
    phase = Deferred => ~taskRetired /\ ~metadataCommitted

RetiredPhaseRequiresPublishedRetirement ==
    phase = Retired => taskRetired

GrowthEventuallySettles ==
    phase \in {Planned, Applying, Deferred, Mapped} ~> phase \in {Committed, Retired}

=============================================================================
