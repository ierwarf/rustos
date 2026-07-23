------------------------ MODULE SyscallSimdLifecycle -------------------------
EXTENDS Naturals

(*******************************************************************************
Models the two distinct SIMD/FPU lifetimes that coexist when a syscall blocks:

  * the exact userspace image captured at the syscall trust boundary; and
  * the scheduler slot holding a suspended kernel continuation's SIMD scratch.

Preemption may freely replace the scheduler image but cannot alter the syscall
snapshot. Return is authorized only for the entering task and restores exactly
that snapshot. Nested capture and a wrong-task restore are rejected.
*******************************************************************************)

CONSTANTS MaxTask, MaxImage

Phases == {"user", "syscall", "blocked", "resumed", "returned"}

VARIABLES phase, currentTask, ownerTask, entryImage, snapshot, schedulerImage,
          returnImage, active, nestedRejected, wrongTaskRejected

vars == <<phase, currentTask, ownerTask, entryImage, snapshot, schedulerImage,
          returnImage, active, nestedRejected, wrongTaskRejected>>

Init ==
    /\ phase = "user"
    /\ currentTask \in 1..MaxTask
    /\ ownerTask = 0
    /\ entryImage \in 1..MaxImage
    /\ snapshot = 0
    /\ schedulerImage = 0
    /\ returnImage = 0
    /\ active = FALSE
    /\ nestedRejected = FALSE
    /\ wrongTaskRejected = FALSE

Enter ==
    /\ phase = "user"
    /\ ~active
    /\ phase' = "syscall"
    /\ ownerTask' = currentTask
    /\ snapshot' = entryImage
    /\ active' = TRUE
    /\ UNCHANGED <<currentTask, entryImage, schedulerImage, returnImage,
                   nestedRejected, wrongTaskRejected>>

Block ==
    /\ phase = "syscall"
    /\ active
    /\ phase' = "blocked"
    /\ schedulerImage' \in 1..MaxImage
    /\ UNCHANGED <<currentTask, ownerTask, entryImage, snapshot, returnImage,
                   active, nestedRejected, wrongTaskRejected>>

KernelScratch ==
    /\ phase \in {"syscall", "blocked", "resumed"}
    /\ active
    /\ schedulerImage' \in 1..MaxImage
    /\ UNCHANGED <<phase, currentTask, ownerTask, entryImage, snapshot,
                   returnImage, active, nestedRejected, wrongTaskRejected>>

ScheduleOther ==
    /\ phase = "blocked"
    /\ active
    /\ MaxTask > 1
    /\ \E task \in 1..MaxTask:
        /\ task # ownerTask
        /\ currentTask' = task
    /\ UNCHANGED <<phase, ownerTask, entryImage, snapshot, schedulerImage,
                   returnImage, active, nestedRejected, wrongTaskRejected>>

RejectWrongTaskReturn ==
    /\ phase \in {"blocked", "resumed"}
    /\ active
    /\ currentTask # ownerTask
    /\ wrongTaskRejected' = TRUE
    /\ UNCHANGED <<phase, currentTask, ownerTask, entryImage, snapshot,
                   schedulerImage, returnImage, active, nestedRejected>>

ScheduleOwner ==
    /\ phase = "blocked"
    /\ active
    /\ currentTask # ownerTask
    /\ currentTask' = ownerTask
    /\ UNCHANGED <<phase, ownerTask, entryImage, snapshot, schedulerImage,
                   returnImage, active, nestedRejected, wrongTaskRejected>>

Resume ==
    /\ phase = "blocked"
    /\ active
    /\ currentTask = ownerTask
    /\ phase' = "resumed"
    /\ UNCHANGED <<currentTask, ownerTask, entryImage, snapshot,
                   schedulerImage, returnImage, active, nestedRejected,
                   wrongTaskRejected>>

RejectNestedCapture ==
    /\ phase \in {"syscall", "blocked", "resumed"}
    /\ active
    /\ nestedRejected' = TRUE
    /\ UNCHANGED <<phase, currentTask, ownerTask, entryImage, snapshot,
                   schedulerImage, returnImage, active, wrongTaskRejected>>

Return ==
    /\ phase \in {"syscall", "resumed"}
    /\ active
    /\ currentTask = ownerTask
    /\ phase' = "returned"
    /\ returnImage' = snapshot
    /\ active' = FALSE
    /\ UNCHANGED <<currentTask, ownerTask, entryImage, snapshot,
                   schedulerImage, nestedRejected, wrongTaskRejected>>

Next ==
    Enter
    \/ Block
    \/ KernelScratch
    \/ ScheduleOther
    \/ RejectWrongTaskReturn
    \/ ScheduleOwner
    \/ Resume
    \/ RejectNestedCapture
    \/ Return

TypeOK ==
    /\ phase \in Phases
    /\ currentTask \in 1..MaxTask
    /\ ownerTask \in 0..MaxTask
    /\ entryImage \in 1..MaxImage
    /\ snapshot \in 0..MaxImage
    /\ schedulerImage \in 0..MaxImage
    /\ returnImage \in 0..MaxImage
    /\ active \in BOOLEAN
    /\ nestedRejected \in BOOLEAN
    /\ wrongTaskRejected \in BOOLEAN

ActiveSnapshotIsEntryImage == active => snapshot = entryImage

ActiveSnapshotHasExactOwner == active => ownerTask \in 1..MaxTask

ReturnRestoresEntryImage ==
    phase = "returned" => returnImage = entryImage

ReturnBelongsToEnteringTask ==
    phase = "returned" => currentTask = ownerTask

ReturnedSnapshotIsInactive ==
    phase = "returned" => ~active

SchedulerScratchCannotAuthorizeReturn ==
    phase = "returned" => returnImage = snapshot

Spec == Init /\ [][Next]_vars
===============================================================================
