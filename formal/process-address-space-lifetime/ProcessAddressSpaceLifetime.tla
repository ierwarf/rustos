------------------ MODULE ProcessAddressSpaceLifetime ------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Owner: kernel/ps process table and UserProcessState.
Linearization point: the per-process state mutex acquired by ProcessRef.
The model covers retained references, exclusive state access, exec mutation,
process exit, and final reclamation. A table reference alone cannot authorize
an unlocked address-space access.
***************************************************************************)

CONSTANTS Tasks, MaxEpoch, MaxThreads
NoTask == "none"
NoEpoch == MaxEpoch + 1
NoThreadCount == MaxThreads + 1
NoneMode == "none"
ReadMode == "read"
WriteMode == "write"
AttachIdle == "idle"
AttachPrepared == "prepared"

VARIABLES alive, threadCount, exitMarked, retained, lockOwner, accessMode,
          epoch, exitEpoch, exitThreadCount, attachState, unpublishedStack,
          rejectedAttachAfterExit, reclaimed
vars == <<alive, threadCount, exitMarked, retained, lockOwner, accessMode,
          epoch, exitEpoch, exitThreadCount, attachState, unpublishedStack,
          rejectedAttachAfterExit, reclaimed>>

Init ==
    /\ alive = TRUE
    /\ threadCount = 1
    /\ exitMarked = FALSE
    /\ retained = {}
    /\ lockOwner = NoTask
    /\ accessMode = NoneMode
    /\ epoch = 0
    /\ exitEpoch = NoEpoch
    /\ exitThreadCount = NoThreadCount
    /\ attachState = AttachIdle
    /\ unpublishedStack = FALSE
    /\ rejectedAttachAfterExit = FALSE
    /\ reclaimed = FALSE

Retain(t) ==
    /\ alive /\ ~reclaimed /\ t \notin retained
    /\ retained' = retained \cup {t}
    /\ UNCHANGED <<alive, threadCount, exitMarked, lockOwner, accessMode,
                    epoch, exitEpoch, exitThreadCount, attachState,
                    unpublishedStack, rejectedAttachAfterExit, reclaimed>>

Release(t) ==
    /\ t \in retained /\ lockOwner # t
    /\ retained' = retained \ {t}
    /\ UNCHANGED <<alive, threadCount, exitMarked, lockOwner, accessMode,
                    epoch, exitEpoch, exitThreadCount, attachState,
                    unpublishedStack, rejectedAttachAfterExit, reclaimed>>

BeginAccess(t, mode) ==
    /\ t \in retained /\ lockOwner = NoTask
    /\ mode \in {ReadMode, WriteMode}
    /\ lockOwner' = t /\ accessMode' = mode
    /\ UNCHANGED <<alive, threadCount, exitMarked, retained, epoch, exitEpoch,
                    exitThreadCount, attachState, unpublishedStack,
                    rejectedAttachAfterExit, reclaimed>>

EndAccess(t) ==
    /\ lockOwner = t
    /\ lockOwner' = NoTask /\ accessMode' = NoneMode
    /\ UNCHANGED <<alive, threadCount, exitMarked, retained, epoch, exitEpoch,
                    exitThreadCount, attachState, unpublishedStack,
                    rejectedAttachAfterExit, reclaimed>>

ExecMutation(t) ==
    /\ alive /\ lockOwner = t /\ accessMode = WriteMode /\ epoch < MaxEpoch
    /\ epoch' = epoch + 1
    /\ UNCHANGED <<alive, threadCount, exitMarked, retained, lockOwner,
                    accessMode, exitEpoch, exitThreadCount, attachState,
                    unpublishedStack, rejectedAttachAfterExit, reclaimed>>

BeginThreadAttach ==
    /\ ~reclaimed /\ attachState = AttachIdle
    /\ attachState' = AttachPrepared /\ unpublishedStack' = TRUE
    /\ UNCHANGED <<alive, threadCount, exitMarked, retained, lockOwner,
                    accessMode, epoch, exitEpoch, exitThreadCount,
                    rejectedAttachAfterExit, reclaimed>>

CommitThreadAttach ==
    /\ attachState = AttachPrepared
    /\ alive /\ ~exitMarked /\ threadCount < MaxThreads
    /\ threadCount' = threadCount + 1
    /\ attachState' = AttachIdle /\ unpublishedStack' = FALSE
    /\ UNCHANGED <<alive, exitMarked, retained, lockOwner, accessMode, epoch,
                    exitEpoch, exitThreadCount, rejectedAttachAfterExit,
                    reclaimed>>

RejectThreadAttach ==
    /\ attachState = AttachPrepared
    /\ (~alive \/ exitMarked \/ threadCount = MaxThreads)
    /\ attachState' = AttachIdle /\ unpublishedStack' = FALSE
    /\ rejectedAttachAfterExit' = (rejectedAttachAfterExit \/ exitMarked)
    /\ UNCHANGED <<alive, threadCount, exitMarked, retained, lockOwner,
                    accessMode, epoch, exitEpoch, exitThreadCount, reclaimed>>

SettleThreadAttach == CommitThreadAttach \/ RejectThreadAttach

DetachLiveThread ==
    /\ alive /\ threadCount > 1
    /\ threadCount' = threadCount - 1
    /\ UNCHANGED <<alive, exitMarked, retained, lockOwner, accessMode, epoch,
                    exitEpoch, exitThreadCount, attachState, unpublishedStack,
                    rejectedAttachAfterExit, reclaimed>>

Exit ==
    /\ alive /\ alive' = FALSE /\ exitMarked' = TRUE /\ exitEpoch' = epoch
    /\ exitThreadCount' = threadCount
    /\ UNCHANGED <<threadCount, retained, lockOwner, accessMode, epoch,
                    attachState, unpublishedStack, rejectedAttachAfterExit,
                    reclaimed>>

DetachExitingThread ==
    /\ ~alive /\ exitMarked /\ threadCount > 0
    /\ threadCount' = threadCount - 1
    /\ UNCHANGED <<alive, exitMarked, retained, lockOwner, accessMode, epoch,
                    exitEpoch, exitThreadCount, attachState, unpublishedStack,
                    rejectedAttachAfterExit, reclaimed>>

Reclaim ==
    /\ ~alive /\ threadCount = 0 /\ retained = {} /\ lockOwner = NoTask
    /\ attachState = AttachIdle /\ ~unpublishedStack
    /\ reclaimed' = TRUE
    /\ UNCHANGED <<alive, threadCount, exitMarked, retained, lockOwner,
                    accessMode, epoch, exitEpoch, exitThreadCount, attachState,
                    unpublishedStack, rejectedAttachAfterExit>>

Next ==
    \/ \E t \in Tasks: Retain(t) \/ Release(t)
    \/ \E t \in Tasks, mode \in {ReadMode, WriteMode}: BeginAccess(t, mode)
    \/ \E t \in Tasks: EndAccess(t) \/ ExecMutation(t)
    \/ BeginThreadAttach \/ CommitThreadAttach \/ RejectThreadAttach
    \/ DetachLiveThread \/ Exit \/ DetachExitingThread \/ Reclaim

Spec == Init /\ [][Next]_vars /\ WF_vars(SettleThreadAttach)

TypeOK ==
    /\ alive \in BOOLEAN /\ threadCount \in 0..MaxThreads
    /\ exitMarked \in BOOLEAN /\ retained \in SUBSET Tasks
    /\ lockOwner \in Tasks \cup {NoTask}
    /\ accessMode \in {NoneMode, ReadMode, WriteMode}
    /\ epoch \in 0..MaxEpoch /\ exitEpoch \in 0..NoEpoch
    /\ exitThreadCount \in 0..NoThreadCount
    /\ attachState \in {AttachIdle, AttachPrepared}
    /\ unpublishedStack \in BOOLEAN /\ rejectedAttachAfterExit \in BOOLEAN
    /\ reclaimed \in BOOLEAN
AccessHasRetainedOwner == lockOwner # NoTask => lockOwner \in retained
LockAndModeAgree == (lockOwner = NoTask) <=> (accessMode = NoneMode)
ReclaimedHasNoAuthority ==
    reclaimed => ~alive /\ threadCount = 0 /\ retained = {} /\ lockOwner = NoTask
                 /\ attachState = AttachIdle /\ ~unpublishedStack
LiveProcessHasNoExitEpoch == alive => exitEpoch = NoEpoch
ExitedAddressSpaceEpochIsFrozen == ~alive => epoch = exitEpoch
ExitMarkerIsMonotonic == exitMarked <=> ~alive
LiveProcessHasAtLeastOneThread == alive => threadCount > 0
AttachPreparationOwnsExactlyOneUnpublishedStack ==
    (attachState = AttachPrepared) <=> unpublishedStack
ExitedProcessCannotGainThreads ==
    exitMarked => threadCount <= exitThreadCount
RejectedExitAttachPreservesTerminalEpoch ==
    rejectedAttachAfterExit => exitMarked /\ ~alive /\ epoch = exitEpoch
PreparedAttachEventuallySettles ==
    attachState = AttachPrepared ~> attachState = AttachIdle

=============================================================================
