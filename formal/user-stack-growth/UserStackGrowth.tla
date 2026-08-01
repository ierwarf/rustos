------------------------- MODULE UserStackGrowth -------------------------
EXTENDS Naturals

(***************************************************************************
Recoverable grow-down faults cross the scheduler and process-state lock
domains.  Planning and metadata commit are scheduler-owned; mapping is
process-state-owned.  A stale plan, contention, exec generation change, or
retirement must terminate only the faulting task, never nest the two locks.
***************************************************************************)

CONSTANT MaxGeneration
NoGeneration == MaxGeneration + 1

Idle == "idle"
Planned == "planned"
Applying == "applying"
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

ApplyContentionFailsClosed ==
    /\ phase = Planned
    /\ phase' = Retired /\ taskRetired' = TRUE
    /\ UNCHANGED <<generation, planGeneration, schedulerRaw, processLock,
                    mappingInstalled, metadataCommitted>>

ConcurrentRetirement ==
    /\ phase \in {Planned, Mapped} /\ ~taskRetired
    /\ taskRetired' = TRUE
    /\ UNCHANGED <<phase, generation, planGeneration, schedulerRaw,
                    processLock, mappingInstalled, metadataCommitted>>

ConcurrentExec ==
    /\ phase \in {Planned, Mapped} /\ ~processLock
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
    \/ Prepare \/ BeginApply \/ FinishApply \/ ApplyContentionFailsClosed
    \/ ConcurrentRetirement \/ ConcurrentExec
    \/ CommitMetadata \/ RejectStaleCommit \/ Terminal

Settle == BeginApply \/ FinishApply \/ ApplyContentionFailsClosed
          \/ CommitMetadata \/ RejectStaleCommit

Spec == Init /\ [][Next]_vars /\ WF_vars(Settle)

TypeOK ==
    /\ phase \in {Idle, Planned, Applying, Mapped, Committed, Retired}
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

GrowthEventuallySettles ==
    phase \in {Planned, Applying, Mapped} ~> phase \in {Committed, Retired}

=============================================================================
