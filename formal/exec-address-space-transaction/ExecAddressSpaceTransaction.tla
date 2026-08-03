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
    /\ phase \in {Finalized, Exiting, Cancelled}
    /\ UNCHANGED vars

Next == ReserveExec \/ BeginStage \/ CompleteStage \/ BeginPublish
        \/ CompletePublish \/ BeginFinalize \/ CompleteFinalize
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
                   Finalizing, Finalized, Exiting, Cancelled}
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
NoNormalPublicationFailure == ~publicationFailed
OldBundleRetainedUntilVisibleCommit ==
    visibleGeneration = OldGeneration => oldBundleRetained
ExitEventuallyWins ==
    exitPending ~> phase \in {Exiting, Cancelled}
ReservedExecEventuallySettles ==
    phase \in {Reserved, Staging, Staged, Publishing, Published, Finalizing}
        ~> phase \in {Finalized, Exiting, Cancelled}

=============================================================================
