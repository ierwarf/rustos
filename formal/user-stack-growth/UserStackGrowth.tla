------------------------- MODULE UserStackGrowth -------------------------
EXTENDS Naturals

(***************************************************************************
The enabled product profile eagerly maps every usable stack page before a
task becomes runnable and leaves one permanent guard page unmapped. There is
no lazy-growth exception transaction: a fault after publication is either the
guard/outside the admitted stack and retires normally, or it exposes a broken
eager-map invariant. A future lazy profile requires a new deferred-fault owner
and a separate refinement before it may enter this model or the source path.
***************************************************************************)

Idle == "idle"
Mapping == "mapping"
Mapped == "mapped"
Running == "running"
Accessed == "accessed"
Retired == "retired"

VARIABLES phase, processLock, schedulerRaw, allUsableMapped, guardMapped,
          taskPublished, usableAccessFaulted, taskRetired

vars == <<phase, processLock, schedulerRaw, allUsableMapped, guardMapped,
          taskPublished, usableAccessFaulted, taskRetired>>

Init ==
    /\ phase = Idle
    /\ processLock = FALSE
    /\ schedulerRaw = FALSE
    /\ allUsableMapped = FALSE
    /\ guardMapped = FALSE
    /\ taskPublished = FALSE
    /\ usableAccessFaulted = FALSE
    /\ taskRetired = FALSE

BeginEagerMap ==
    /\ phase = Idle
    /\ phase' = Mapping
    /\ processLock' = TRUE
    /\ UNCHANGED <<schedulerRaw, allUsableMapped, guardMapped, taskPublished,
                    usableAccessFaulted, taskRetired>>

CompleteEagerMap ==
    /\ phase = Mapping
    /\ processLock
    /\ phase' = Mapped
    /\ processLock' = FALSE
    /\ allUsableMapped' = TRUE
    /\ guardMapped' = FALSE
    /\ UNCHANGED <<schedulerRaw, taskPublished, usableAccessFaulted,
                    taskRetired>>

PublishTask ==
    /\ phase = Mapped
    /\ allUsableMapped
    /\ ~guardMapped
    /\ phase' = Running
    /\ schedulerRaw' = TRUE
    /\ taskPublished' = TRUE
    /\ UNCHANGED <<processLock, allUsableMapped, guardMapped,
                    usableAccessFaulted, taskRetired>>

ReleaseScheduler ==
    /\ phase = Running
    /\ schedulerRaw
    /\ schedulerRaw' = FALSE
    /\ UNCHANGED <<phase, processLock, allUsableMapped, guardMapped,
                    taskPublished, usableAccessFaulted, taskRetired>>

AccessUsableStack ==
    /\ phase = Running
    /\ taskPublished
    /\ ~schedulerRaw
    /\ phase' = Accessed
    /\ usableAccessFaulted' = ~allUsableMapped
    /\ taskRetired' = ~allUsableMapped
    /\ UNCHANGED <<processLock, schedulerRaw, allUsableMapped, guardMapped,
                    taskPublished>>

FaultPermanentGuard ==
    /\ phase = Running
    /\ taskPublished
    /\ ~schedulerRaw
    /\ phase' = Retired
    /\ taskRetired' = TRUE
    /\ UNCHANGED <<processLock, schedulerRaw, allUsableMapped, guardMapped,
                    taskPublished, usableAccessFaulted>>

Terminal ==
    /\ phase \in {Accessed, Retired}
    /\ UNCHANGED vars

Next == BeginEagerMap \/ CompleteEagerMap \/ PublishTask
        \/ ReleaseScheduler \/ AccessUsableStack \/ FaultPermanentGuard
        \/ Terminal

Settle == BeginEagerMap \/ CompleteEagerMap \/ PublishTask
          \/ ReleaseScheduler \/ AccessUsableStack \/ FaultPermanentGuard

Spec == Init /\ [][Next]_vars /\ WF_vars(Settle)

TypeOK ==
    /\ phase \in {Idle, Mapping, Mapped, Running, Accessed, Retired}
    /\ processLock \in BOOLEAN
    /\ schedulerRaw \in BOOLEAN
    /\ allUsableMapped \in BOOLEAN
    /\ guardMapped \in BOOLEAN
    /\ taskPublished \in BOOLEAN
    /\ usableAccessFaulted \in BOOLEAN
    /\ taskRetired \in BOOLEAN

LockDomainsNeverOverlap == ~(schedulerRaw /\ processLock)
PublishedTaskHasCompleteEagerStack ==
    taskPublished => allUsableMapped /\ ~guardMapped
MappedStateHasCompleteEagerStack ==
    phase \in {Mapped, Running, Accessed, Retired} =>
        allUsableMapped /\ ~guardMapped
UsableStackAccessNeverFaults == ~usableAccessFaulted
RetirementIsOnlyARealFault ==
    taskRetired => phase = Retired \/ usableAccessFaulted
NoLazyGrowthPhase ==
    phase \in {Idle, Mapping, Mapped, Running, Accessed, Retired}

StartupEventuallySettles ==
    phase \in {Idle, Mapping, Mapped, Running} ~> phase \in {Accessed, Retired}

=============================================================================
