------------------- MODULE ExecAddressSpaceTransaction -------------------
EXTENDS Naturals

(***************************************************************************
Exec owns one scheduler reservation token. ProcessState staging and final
visibility occur under the sleepable ProcessState lock, while scheduler
publication occurs under the raw scheduler owner; the two locks never nest.
Exit during the transaction latches exitPending instead of retiring the
reserved target. Readers reject the staged interval, and the new generation
becomes visible only after scheduler publication.
***************************************************************************)

Idle == "idle"
Reserved == "reserved"
Staging == "staging"
Staged == "staged"
Publishing == "publishing"
Published == "published"
Finalizing == "finalizing"
Finalized == "finalized"
Exiting == "exiting"
Cancelled == "cancelled"
PublishFailed == "publish-failed"

OldGeneration == 0
NewGeneration == 1

VARIABLES phase, tokenLive, tokenUses, stagedBundle, schedulerPublished,
          visibleGeneration, exitPending, targetRetired, oldBundleRetained,
          processStateLockHeld, schedulerLockHeld, locksNested,
          publicationFailed

vars == <<phase, tokenLive, tokenUses, stagedBundle, schedulerPublished,
          visibleGeneration, exitPending, targetRetired, oldBundleRetained,
          processStateLockHeld, schedulerLockHeld, locksNested,
          publicationFailed>>

Init ==
    /\ phase = Idle
    /\ tokenLive = FALSE
    /\ tokenUses = 0
    /\ stagedBundle = FALSE
    /\ schedulerPublished = FALSE
    /\ visibleGeneration = OldGeneration
    /\ exitPending = FALSE
    /\ targetRetired = FALSE
    /\ oldBundleRetained = TRUE
    /\ processStateLockHeld = FALSE
    /\ schedulerLockHeld = FALSE
    /\ locksNested = FALSE
    /\ publicationFailed = FALSE

ReserveExec ==
    /\ phase = Idle
    /\ phase' = Reserved
    /\ tokenLive' = TRUE
    /\ UNCHANGED <<tokenUses, stagedBundle, schedulerPublished,
                    visibleGeneration, exitPending, targetRetired,
                    oldBundleRetained, processStateLockHeld,
                    schedulerLockHeld, locksNested, publicationFailed>>

BeginStage ==
    /\ phase = Reserved
    /\ tokenLive
    /\ ~schedulerLockHeld
    /\ phase' = Staging
    /\ processStateLockHeld' = TRUE
    /\ locksNested' = schedulerLockHeld
    /\ UNCHANGED <<tokenLive, tokenUses, stagedBundle, schedulerPublished,
                    visibleGeneration, exitPending, targetRetired,
                    oldBundleRetained, schedulerLockHeld, publicationFailed>>

CompleteStage ==
    /\ phase = Staging
    /\ processStateLockHeld
    /\ phase' = Staged
    /\ stagedBundle' = TRUE
    /\ processStateLockHeld' = FALSE
    /\ UNCHANGED <<tokenLive, tokenUses, schedulerPublished,
                    visibleGeneration, exitPending, targetRetired,
                    oldBundleRetained, schedulerLockHeld, locksNested,
                    publicationFailed>>

BeginPublish ==
    /\ phase = Staged
    /\ tokenLive
    /\ stagedBundle
    /\ ~processStateLockHeld
    /\ ~targetRetired
    /\ phase' = Publishing
    /\ schedulerLockHeld' = TRUE
    /\ locksNested' = processStateLockHeld
    /\ UNCHANGED <<tokenLive, tokenUses, stagedBundle, schedulerPublished,
                    visibleGeneration, exitPending, targetRetired,
                    oldBundleRetained, processStateLockHeld,
                    publicationFailed>>

CompletePublish ==
    /\ phase = Publishing
    /\ schedulerLockHeld
    /\ tokenLive
    /\ tokenUses = 0
    /\ phase' = Published
    /\ schedulerPublished' = TRUE
    /\ tokenUses' = 1
    /\ schedulerLockHeld' = FALSE
    /\ UNCHANGED <<tokenLive, stagedBundle, visibleGeneration, exitPending,
                    targetRetired, oldBundleRetained, processStateLockHeld,
                    locksNested, publicationFailed>>

(***************************************************************************
Publication is source-level fail-stop today: the reserved target is expected
to remain exact, and losing it is a kernel invariant violation. Model that
failure as a reachable terminal rollback instead of initializing a Boolean
that no action could ever change. No new generation becomes visible, the old
bundle remains authoritative, and an already-latched exit still retires the
target. This action makes the failure proof non-vacuous without pretending
that normal execution can recover after the invariant fault.
***************************************************************************)
FailPublish ==
    /\ phase = Publishing
    /\ schedulerLockHeld
    /\ tokenLive
    /\ tokenUses = 0
    /\ phase' = IF exitPending THEN Exiting ELSE PublishFailed
    /\ tokenLive' = FALSE
    /\ stagedBundle' = FALSE
    /\ schedulerLockHeld' = FALSE
    /\ publicationFailed' = TRUE
    /\ targetRetired' = exitPending
    /\ UNCHANGED <<tokenUses, schedulerPublished, visibleGeneration,
                    exitPending, oldBundleRetained, processStateLockHeld,
                    locksNested>>

BeginFinalize ==
    /\ phase = Published
    /\ schedulerPublished
    /\ ~schedulerLockHeld
    /\ phase' = Finalizing
    /\ processStateLockHeld' = TRUE
    /\ locksNested' = schedulerLockHeld
    /\ UNCHANGED <<tokenLive, tokenUses, stagedBundle,
                    schedulerPublished, visibleGeneration, exitPending,
                    targetRetired, oldBundleRetained, schedulerLockHeld,
                    publicationFailed>>

CompleteFinalize ==
    /\ phase = Finalizing
    /\ processStateLockHeld
    /\ schedulerPublished
    /\ phase' = IF exitPending THEN Exiting ELSE Finalized
    /\ visibleGeneration' = NewGeneration
    /\ tokenLive' = FALSE
    /\ stagedBundle' = FALSE
    /\ oldBundleRetained' = FALSE
    /\ targetRetired' = exitPending
    /\ processStateLockHeld' = FALSE
    /\ UNCHANGED <<tokenUses, schedulerPublished, exitPending,
                    schedulerLockHeld, locksNested, publicationFailed>>

RequestExit ==
    /\ phase \in {Reserved, Staging, Staged, Publishing, Published, Finalizing}
    /\ ~exitPending
    /\ exitPending' = TRUE
    /\ UNCHANGED <<phase, tokenLive, tokenUses, stagedBundle,
                    schedulerPublished, visibleGeneration, targetRetired,
                    oldBundleRetained, processStateLockHeld,
                    schedulerLockHeld, locksNested, publicationFailed>>

CancelBeforeStage ==
    /\ phase = Reserved
    /\ exitPending
    /\ phase' = Cancelled
    /\ tokenLive' = FALSE
    /\ targetRetired' = TRUE
    /\ UNCHANGED <<tokenUses, stagedBundle, schedulerPublished,
                    visibleGeneration, exitPending, oldBundleRetained,
                    processStateLockHeld, schedulerLockHeld, locksNested,
                    publicationFailed>>

Terminal ==
    /\ phase \in {Finalized, Exiting, Cancelled, PublishFailed}
    /\ UNCHANGED vars

Next == ReserveExec \/ BeginStage \/ CompleteStage \/ BeginPublish
        \/ CompletePublish \/ FailPublish \/ BeginFinalize \/ CompleteFinalize
        \/ RequestExit \/ CancelBeforeStage \/ Terminal

Spec ==
    Init /\ [][Next]_vars
    /\ WF_vars(BeginStage)
    /\ WF_vars(CompleteStage)
    /\ WF_vars(BeginPublish)
    /\ WF_vars(CompletePublish)
    /\ WF_vars(BeginFinalize)
    /\ WF_vars(CompleteFinalize)
    /\ WF_vars(CancelBeforeStage)

TypeOK ==
    /\ phase \in {Idle, Reserved, Staging, Staged, Publishing, Published,
                   Finalizing, Finalized, Exiting, Cancelled, PublishFailed}
    /\ tokenLive \in BOOLEAN
    /\ tokenUses \in 0..1
    /\ stagedBundle \in BOOLEAN
    /\ schedulerPublished \in BOOLEAN
    /\ visibleGeneration \in {OldGeneration, NewGeneration}
    /\ exitPending \in BOOLEAN
    /\ targetRetired \in BOOLEAN
    /\ oldBundleRetained \in BOOLEAN
    /\ processStateLockHeld \in BOOLEAN
    /\ schedulerLockHeld \in BOOLEAN
    /\ locksNested \in BOOLEAN
    /\ publicationFailed \in BOOLEAN

NoVisibleHalfExec ==
    visibleGeneration = NewGeneration =>
        /\ schedulerPublished
        /\ phase \in {Finalized, Exiting}

NoCommitAfterRetire ==
    targetRetired => ~tokenLive /\ phase \in {Exiting, Cancelled}

TokenSingleUse == tokenUses <= 1
ProcessAndSchedulerLocksNeverNest ==
    /\ ~locksNested
    /\ ~(processStateLockHeld /\ schedulerLockHeld)
PublicationFailureRollsBack ==
    publicationFailed =>
        /\ phase \in {PublishFailed, Exiting}
        /\ ~tokenLive
        /\ ~stagedBundle
        /\ ~schedulerPublished
        /\ visibleGeneration = OldGeneration
        /\ oldBundleRetained
OldBundleRetainedUntilVisibleCommit ==
    visibleGeneration = OldGeneration => oldBundleRetained
ExitEventuallyWins ==
    exitPending ~> phase \in {Exiting, Cancelled}
ReservedExecEventuallySettles ==
    phase \in {Reserved, Staging, Staged, Publishing, Published, Finalizing}
        ~> phase \in {Finalized, Exiting, Cancelled, PublishFailed}

=============================================================================
