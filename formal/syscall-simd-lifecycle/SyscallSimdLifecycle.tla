------------------------ MODULE SyscallSimdLifecycle -------------------------
EXTENDS Naturals

(*******************************************************************************
Models the three distinct FPU/SIMD lifetimes that coexist when a syscall blocks.

  * The narrow image -- the sixteen XMM registers -- which the entry stub saves
    to the entering task's own kernel stack before the first Rust instruction,
    and reloads immediately before SYSRET.  The stack is the save area, so the
    image cannot be nested over, cannot be reached by another task, and rides an
    arbitrarily long blocked continuation for free.

  * The scheduler slot holding a suspended kernel continuation's SIMD scratch.
    Preemption may freely replace it; it can never authorize a return.

  * The wide state -- x87, MXCSR, and the ymm upper halves -- which *no* kernel
    entry path saves.  A legacy `movdqu` restore preserves ymm bits 255:128
    rather than restoring them, and every VEX-encoded instruction rewrites those
    bits, a 128-bit one by zeroing them.  So the wide state is held by an
    invariant rather than by a save: kernel code may disturb it only inside an
    explicit bracket that puts it back.

This replaces a per-syscall XSAVE/XRSTOR pair that covered all three at once,
measured at 829 ticks of every syscall.  What made the pair removable is that
two of the three gaps it covered are empty: the shipped image contains no x87
instruction and no floating-point arithmetic, checked on every build by
`tools/xtask/src/build/nucleus_audit.rs`.  The third is not empty -- the ed25519
epoch-signature verification is roughly three thousand VEX instructions -- and is
what `BracketRestoresWideState` below models.
*******************************************************************************)

CONSTANTS MaxTask, MaxImage, MaxBracketDepth

Phases == {"user", "syscall", "blocked", "resumed", "returned"}

VARIABLES phase, currentTask, ownerTask, entryImage, stackImage, schedulerImage,
          returnImage, active, entryWideState, wideState, bracketDepth,
          bracketSaved, syscallFrameLive, continuationPublished, returnValidated

vars == <<phase, currentTask, ownerTask, entryImage, stackImage, schedulerImage,
          returnImage, active, entryWideState, wideState, bracketDepth,
          bracketSaved, syscallFrameLive, continuationPublished,
          returnValidated>>

Init ==
    /\ phase = "user"
    /\ currentTask \in 1..MaxTask
    /\ ownerTask = 0
    /\ entryImage \in 1..MaxImage
    /\ stackImage = 0
    /\ schedulerImage = 0
    /\ returnImage = 0
    /\ active = FALSE
    /\ entryWideState \in 1..MaxImage
    /\ wideState = entryWideState
    /\ bracketDepth = 0
    /\ bracketSaved = [level \in 1..MaxBracketDepth |-> 0]
    /\ syscallFrameLive = FALSE
    /\ continuationPublished = FALSE
    /\ returnValidated = FALSE

(*******************************************************************************
The entry stub saves the narrow image and nothing else.  It does not touch the
wide state, and does not need to: outside a bracket the wide state is still
exactly what userspace left.
*******************************************************************************)
Enter ==
    /\ phase = "user"
    /\ ~active
    /\ phase' = "syscall"
    /\ ownerTask' = currentTask
    /\ stackImage' = entryImage
    /\ active' = TRUE
    /\ syscallFrameLive' = TRUE
    /\ continuationPublished' = FALSE
    /\ returnValidated' = FALSE
    /\ UNCHANGED <<currentTask, entryImage, schedulerImage, returnImage,
                   entryWideState, wideState, bracketDepth, bracketSaved>>

Block ==
    /\ phase = "syscall"
    /\ active
    /\ bracketDepth = 0
    /\ phase' = "blocked"
    /\ schedulerImage' \in 1..MaxImage
    /\ continuationPublished' = TRUE
    /\ returnValidated' = FALSE
    /\ UNCHANGED <<currentTask, ownerTask, entryImage, stackImage, returnImage,
                   active, entryWideState, wideState, bracketDepth,
                   bracketSaved, syscallFrameLive>>

(*******************************************************************************
Ordinary kernel code.  It may use XMM scratch freely -- the narrow image is
already on the stack -- but it may not reach the wide state.  That is the
property `nucleus_audit.rs` enforces on the linked image, and its absence here
is the whole reason the XSAVE could go.
*******************************************************************************)
KernelScratch ==
    /\ phase \in {"syscall", "blocked", "resumed"}
    /\ active
    /\ schedulerImage' \in 1..MaxImage
    /\ UNCHANGED <<phase, currentTask, ownerTask, entryImage, stackImage,
                   returnImage, active, entryWideState, wideState,
                   bracketDepth, bracketSaved, syscallFrameLive,
                   continuationPublished, returnValidated>>

EnterWideSection ==
    /\ phase \in {"syscall", "resumed"}
    /\ active
    /\ bracketDepth < MaxBracketDepth
    /\ bracketDepth' = bracketDepth + 1
    /\ bracketSaved' = [bracketSaved EXCEPT ![bracketDepth + 1] = wideState]
    /\ UNCHANGED <<phase, currentTask, ownerTask, entryImage, stackImage,
                   schedulerImage, returnImage, active, entryWideState,
                   wideState, syscallFrameLive, continuationPublished,
                   returnValidated>>

WideScratch ==
    /\ bracketDepth > 0
    /\ wideState' \in 1..MaxImage
    /\ UNCHANGED <<phase, currentTask, ownerTask, entryImage, stackImage,
                   schedulerImage, returnImage, active, entryWideState,
                   bracketDepth, bracketSaved, syscallFrameLive,
                   continuationPublished, returnValidated>>

LeaveWideSection ==
    /\ bracketDepth > 0
    /\ wideState' = bracketSaved[bracketDepth]
    /\ bracketDepth' = bracketDepth - 1
    /\ UNCHANGED <<phase, currentTask, ownerTask, entryImage, stackImage,
                   schedulerImage, returnImage, active, entryWideState,
                   bracketSaved, syscallFrameLive, continuationPublished,
                   returnValidated>>

ScheduleOther ==
    /\ phase = "blocked"
    /\ active
    /\ MaxTask > 1
    /\ \E task \in 1..MaxTask:
        /\ task # ownerTask
        /\ currentTask' = task
    /\ UNCHANGED <<phase, ownerTask, entryImage, stackImage, schedulerImage,
                   returnImage, active, entryWideState, wideState,
                   bracketDepth, bracketSaved, syscallFrameLive,
                   continuationPublished, returnValidated>>

ScheduleOwner ==
    /\ phase = "blocked"
    /\ active
    /\ currentTask # ownerTask
    /\ currentTask' = ownerTask
    /\ UNCHANGED <<phase, ownerTask, entryImage, stackImage, schedulerImage,
                   returnImage, active, entryWideState, wideState,
                   bracketDepth, bracketSaved, syscallFrameLive,
                   continuationPublished, returnValidated>>

Resume ==
    /\ phase = "blocked"
    /\ active
    /\ currentTask = ownerTask
    /\ phase' = "resumed"
    /\ continuationPublished' = FALSE
    /\ returnValidated' = FALSE
    /\ UNCHANGED <<currentTask, ownerTask, entryImage, stackImage,
                   schedulerImage, returnImage, active, entryWideState,
                   wideState, bracketDepth, bracketSaved, syscallFrameLive>>

(*******************************************************************************
The return contract is checked after the last possible continuation resume.
Checking before a deferred tail reschedule is insufficient: that schedule
publishes and later consumes a kernel frame while the syscall frame remains
live on the owner stack.  SYSRET may consume it only after post-resume
canonical-address/RFLAGS validation.
*******************************************************************************)
ValidateReturn ==
    /\ phase \in {"syscall", "resumed"}
    /\ active
    /\ currentTask = ownerTask
    /\ syscallFrameLive
    /\ ~continuationPublished
    /\ returnValidated' = TRUE
    /\ UNCHANGED <<phase, currentTask, ownerTask, entryImage, stackImage,
                   schedulerImage, returnImage, active, entryWideState,
                   wideState, bracketDepth, bracketSaved, syscallFrameLive,
                   continuationPublished>>

Return ==
    /\ phase \in {"syscall", "resumed"}
    /\ active
    /\ currentTask = ownerTask
    /\ syscallFrameLive
    /\ ~continuationPublished
    /\ returnValidated
    /\ bracketDepth = 0
    /\ phase' = "returned"
    /\ returnImage' = stackImage
    /\ active' = FALSE
    /\ syscallFrameLive' = FALSE
    /\ UNCHANGED <<currentTask, ownerTask, entryImage, stackImage,
                   schedulerImage, entryWideState, wideState, bracketDepth,
                   bracketSaved, continuationPublished, returnValidated>>

Next ==
    Enter
    \/ Block
    \/ KernelScratch
    \/ EnterWideSection
    \/ WideScratch
    \/ LeaveWideSection
    \/ ScheduleOther
    \/ ScheduleOwner
    \/ Resume
    \/ ValidateReturn
    \/ Return

TypeOK ==
    /\ phase \in Phases
    /\ currentTask \in 1..MaxTask
    /\ ownerTask \in 0..MaxTask
    /\ entryImage \in 1..MaxImage
    /\ stackImage \in 0..MaxImage
    /\ schedulerImage \in 0..MaxImage
    /\ returnImage \in 0..MaxImage
    /\ active \in BOOLEAN
    /\ entryWideState \in 1..MaxImage
    /\ wideState \in 1..MaxImage
    /\ bracketDepth \in 0..MaxBracketDepth
    /\ bracketSaved \in [1..MaxBracketDepth -> 0..MaxImage]
    /\ syscallFrameLive \in BOOLEAN
    /\ continuationPublished \in BOOLEAN
    /\ returnValidated \in BOOLEAN

ActiveStackImageIsEntryImage == active => stackImage = entryImage

ActiveStackImageHasExactOwner == active => ownerTask \in 1..MaxTask

ReturnRestoresEntryImage ==
    phase = "returned" => returnImage = entryImage

ReturnBelongsToEnteringTask ==
    phase = "returned" => currentTask = ownerTask

ReturnedStackImageIsInactive ==
    phase = "returned" => ~active

SchedulerScratchCannotAuthorizeReturn ==
    phase = "returned" => returnImage = stackImage

(*******************************************************************************
The state no entry path saves is unchanged wherever no bracket is open.  This
is the invariant that pays for deleting the per-syscall XSAVE: the pair used to
restore the wide state on the way out, and now nothing does, so nothing may
disturb it in the first place.
*******************************************************************************)
UnsavedStateSurvivesUnbracketedKernelCode ==
    bracketDepth = 0 => wideState = entryWideState

BracketRestoresWideState ==
    phase = "returned" => wideState = entryWideState

ActiveSyscallRetainsItsEntryFrame ==
    active => syscallFrameLive

BlockedContinuationOwnsPublishedFrame ==
    phase = "blocked" => continuationPublished

ExecutingContinuationOwnsNoPublishedFrame ==
    phase \in {"syscall", "resumed"} => ~continuationPublished

PublishedContinuationIsNotExecuting ==
    continuationPublished => phase = "blocked"

ReturnConsumesOnlyPostResumeValidatedFrame ==
    phase = "returned" =>
        /\ returnValidated
        /\ ~syscallFrameLive
        /\ ~continuationPublished

Spec == Init /\ [][Next]_vars
===============================================================================
