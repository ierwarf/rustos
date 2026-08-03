------------------- MODULE ExecAddressSpaceTransaction -------------------
EXTENDS Naturals

(***************************************************************************
Exec spans process-table reservation, target quiescence, ProcessState/FD/MM
generation replacement, scheduler root/context publication, and old-bundle
retirement.

The ProcessState lock remains held across the process-generation and scheduler
publication edges. A remote target is not runnable until the last scheduler
edge; self publication excludes IRQs. The old bundle remains retained until
the new root/context is active.

Concrete owners:
  * kernel/ps/src/multitask/{current,process_table,scheduler}.rs
  * kernel/ps/src/user/process_state.rs
***************************************************************************)

Idle == "idle"
Reserved == "reserved"
Quiesced == "quiesced"
Authorized == "authorized"
StateCommitted == "state-committed"
SchedulerPublished == "scheduler-published"
Owned == "owned"
Cancelled == "cancelled"

OldGeneration == 0
NewGeneration == 1

VARIABLES phase, exiting, retiredMarker, reservationValid,
          processGeneration, schedulerGeneration, activeRootGeneration,
          targetRunnable, processStateLockHeld, oldBundleRetained,
          ownershipCommitted, publishedOverRetirement

vars == <<phase, exiting, retiredMarker, reservationValid,
          processGeneration, schedulerGeneration, activeRootGeneration,
          targetRunnable, processStateLockHeld, oldBundleRetained,
          ownershipCommitted, publishedOverRetirement>>

Init ==
    /\ phase = Idle
    /\ exiting = FALSE
    /\ retiredMarker = FALSE
    /\ reservationValid = FALSE
    /\ processGeneration = OldGeneration
    /\ schedulerGeneration = OldGeneration
    /\ activeRootGeneration = OldGeneration
    /\ targetRunnable = TRUE
    /\ processStateLockHeld = FALSE
    /\ oldBundleRetained = TRUE
    /\ ownershipCommitted = FALSE
    /\ publishedOverRetirement = FALSE

BeginExec ==
    /\ phase = Idle /\ ~exiting /\ ~retiredMarker
    /\ phase' = Reserved
    /\ reservationValid' = TRUE
    /\ UNCHANGED <<exiting, retiredMarker, processGeneration,
                    schedulerGeneration, activeRootGeneration, targetRunnable,
                    processStateLockHeld, oldBundleRetained,
                    ownershipCommitted, publishedOverRetirement>>

Quiesce ==
    /\ phase = Reserved
    /\ phase' = Quiesced
    /\ targetRunnable' = FALSE
    /\ UNCHANGED <<exiting, retiredMarker, reservationValid,
                    processGeneration, schedulerGeneration,
                    activeRootGeneration, processStateLockHeld,
                    oldBundleRetained, ownershipCommitted,
                    publishedOverRetirement>>

PublishExit ==
    /\ phase \in {Reserved, Quiesced, Authorized}
    /\ exiting' = TRUE
    /\ retiredMarker' = TRUE
    /\ UNCHANGED <<phase, reservationValid, processGeneration,
                    schedulerGeneration, activeRootGeneration, targetRunnable,
                    processStateLockHeld, oldBundleRetained,
                    ownershipCommitted, publishedOverRetirement>>

Authorize ==
    /\ phase = Quiesced /\ reservationValid
    /\ ~exiting /\ ~retiredMarker
    /\ phase' = Authorized
    /\ UNCHANGED <<exiting, retiredMarker, reservationValid,
                    processGeneration, schedulerGeneration,
                    activeRootGeneration, targetRunnable,
                    processStateLockHeld, oldBundleRetained,
                    ownershipCommitted, publishedOverRetirement>>

CommitProcessState ==
    /\ phase = Authorized /\ reservationValid
    /\ phase' = StateCommitted
    /\ processGeneration' = NewGeneration
    /\ processStateLockHeld' = TRUE
    /\ UNCHANGED <<exiting, retiredMarker, reservationValid,
                    schedulerGeneration, activeRootGeneration, targetRunnable,
                    oldBundleRetained, ownershipCommitted,
                    publishedOverRetirement>>

PublishScheduler ==
    /\ phase = StateCommitted
    /\ reservationValid /\ processStateLockHeld
    /\ processGeneration = NewGeneration
    /\ phase' = SchedulerPublished
    /\ schedulerGeneration' = NewGeneration
    /\ activeRootGeneration' = NewGeneration
    /\ targetRunnable' = TRUE
    /\ publishedOverRetirement' = retiredMarker
    /\ UNCHANGED <<exiting, retiredMarker, reservationValid,
                    processGeneration, processStateLockHeld,
                    oldBundleRetained, ownershipCommitted>>

FinalizeOwnership ==
    /\ phase = SchedulerPublished
    /\ processStateLockHeld
    /\ processGeneration = NewGeneration
    /\ schedulerGeneration = NewGeneration
    /\ activeRootGeneration = NewGeneration
    /\ phase' = Owned
    /\ reservationValid' = FALSE
    /\ processStateLockHeld' = FALSE
    /\ oldBundleRetained' = FALSE
    /\ ownershipCommitted' = TRUE
    /\ UNCHANGED <<exiting, retiredMarker, processGeneration,
                    schedulerGeneration, activeRootGeneration,
                    targetRunnable, publishedOverRetirement>>

CancelBeforeCommit ==
    /\ phase \in {Reserved, Quiesced}
    /\ exiting \/ retiredMarker
    /\ phase' = Cancelled
    /\ reservationValid' = FALSE
    /\ targetRunnable' = TRUE
    /\ UNCHANGED <<exiting, retiredMarker, processGeneration,
                    schedulerGeneration, activeRootGeneration,
                    processStateLockHeld, oldBundleRetained,
                    ownershipCommitted, publishedOverRetirement>>

Terminal ==
    /\ phase \in {Owned, Cancelled}
    /\ UNCHANGED vars

Next ==
    BeginExec \/ Quiesce \/ PublishExit \/ Authorize \/ CommitProcessState
    \/ PublishScheduler \/ FinalizeOwnership \/ CancelBeforeCommit \/ Terminal

Spec ==
    Init /\ [][Next]_vars
    /\ WF_vars(CommitProcessState)
    /\ WF_vars(PublishScheduler)
    /\ WF_vars(FinalizeOwnership)

TypeOK ==
    /\ phase \in {Idle, Reserved, Quiesced, Authorized, StateCommitted,
                   SchedulerPublished, Owned, Cancelled}
    /\ exiting \in BOOLEAN
    /\ retiredMarker \in BOOLEAN
    /\ reservationValid \in BOOLEAN
    /\ processGeneration \in {OldGeneration, NewGeneration}
    /\ schedulerGeneration \in {OldGeneration, NewGeneration}
    /\ activeRootGeneration \in {OldGeneration, NewGeneration}
    /\ targetRunnable \in BOOLEAN
    /\ processStateLockHeld \in BOOLEAN
    /\ oldBundleRetained \in BOOLEAN
    /\ ownershipCommitted \in BOOLEAN
    /\ publishedOverRetirement \in BOOLEAN

RunnableNewRootImpliesCompleteNewGeneration ==
    targetRunnable /\ schedulerGeneration = NewGeneration =>
        /\ processGeneration = NewGeneration
        /\ activeRootGeneration = NewGeneration

MixedGenerationIsNeverExternallyRunnable ==
    processGeneration # schedulerGeneration =>
        processStateLockHeld /\ ~targetRunnable

OldBundleRetainedUntilSchedulerPublication ==
    schedulerGeneration = OldGeneration => oldBundleRetained

OldBundleReleaseRequiresCompletePublication ==
    ~oldBundleRetained =>
        /\ processGeneration = NewGeneration
        /\ schedulerGeneration = NewGeneration
        /\ activeRootGeneration = NewGeneration
        /\ ownershipCommitted

AuthorizedPublishOverRetirementIsGenerationComplete ==
    publishedOverRetirement =>
        /\ phase \in {SchedulerPublished, Owned}
        /\ processGeneration = NewGeneration
        /\ schedulerGeneration = NewGeneration
        /\ activeRootGeneration = NewGeneration

OwnedPhaseIsGenerationComplete ==
    phase = Owned =>
        /\ ownershipCommitted
        /\ ~reservationValid
        /\ ~processStateLockHeld
        /\ targetRunnable
        /\ processGeneration = NewGeneration
        /\ schedulerGeneration = NewGeneration

AuthorizedExecEventuallySettles ==
    phase \in {Authorized, StateCommitted, SchedulerPublished} ~>
        phase = Owned

=============================================================================
