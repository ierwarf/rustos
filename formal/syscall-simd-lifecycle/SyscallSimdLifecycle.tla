------------------------ MODULE SyscallSimdLifecycle -------------------------
EXTENDS Naturals

(***************************************************************************
Ordinary syscalls retain the task-stack XMM image and full scheduler custody.
Register IPC explicitly clobbers FP/vector DATA, not the entering controls.
Its exact-task marker permits only admitted lane save/restore elision, and
only with no open wide section. Interrupt/asynchronous suspension always
captures/restores the full continuation, including an open wide section.

A lane may inherit arbitrary peer data AND controls. Both SYSRET and the
published-user IRET route must sanitize data and restore original controls
before returning or capturing a user image. Marker retirement and user-frame
publication occur masked; a recycled task cannot inherit an active marker.
The linked-image audit excludes ordinary x87/FP arithmetic and requires wide
kernel work to bracket itself. Interrupt prefixes still preserve kernel XMM.
***************************************************************************)
CONSTANTS MaxTask, MaxImage, MaxBracketDepth
Phases == {"user", "syscall", "blocked", "async", "async-resumed", "resumed", "finalized",
           "published-user", "returned", "retired"}
VARIABLES phase, currentTask, ownerTask, entryImage, stackImage, schedulerImage,
          returnImage, active, entryWideState, wideState, bracketDepth,
          bracketSaved, syscallFrameLive, continuationPublished, returnValidated,
          registerMode, marker, entryControl, control, asyncWide, asyncControl,
          userWide, userControl
vars == <<phase, currentTask, ownerTask, entryImage, stackImage, schedulerImage,
          returnImage, active, entryWideState, wideState, bracketDepth,
          bracketSaved, syscallFrameLive, continuationPublished, returnValidated,
          registerMode, marker, entryControl, control, asyncWide, asyncControl,
          userWide, userControl>>

Init ==
    /\ phase = "user"
    /\ currentTask \in 1..MaxTask
    /\ ownerTask = 0
    /\ entryImage \in 1..MaxImage
    /\ stackImage = 0 /\ schedulerImage = 0 /\ returnImage = 0
    /\ active = FALSE
    /\ entryWideState \in 1..MaxImage /\ wideState = entryWideState
    /\ bracketDepth = 0
    /\ bracketSaved = [level \in 1..MaxBracketDepth |-> 0]
    /\ syscallFrameLive = FALSE /\ continuationPublished = FALSE
    /\ returnValidated = FALSE
    /\ registerMode \in BOOLEAN /\ marker = 0
    /\ entryControl \in 1..MaxImage /\ control = entryControl
    /\ asyncWide = 0 /\ asyncControl = 0
    /\ userWide = 0 /\ userControl = 0

Enter ==
    /\ phase = "user" /\ ~active /\ marker = 0
    /\ phase' = "syscall" /\ ownerTask' = currentTask
    /\ stackImage' = IF registerMode THEN 0 ELSE entryImage
    /\ marker' = IF registerMode THEN currentTask ELSE 0
    /\ active' = TRUE /\ syscallFrameLive' = TRUE
    /\ UNCHANGED <<currentTask, entryImage, schedulerImage, returnImage,
        entryWideState, wideState, bracketDepth, bracketSaved,
        continuationPublished, returnValidated, registerMode, entryControl,
        control, asyncWide, asyncControl, userWide, userControl>>

Block ==
    /\ phase \in {"syscall", "resumed"} /\ active
    /\ currentTask = ownerTask /\ bracketDepth = 0
    /\ phase' = "blocked" /\ continuationPublished' = TRUE
    /\ returnValidated' = FALSE
    /\ schedulerImage' = IF registerMode THEN schedulerImage ELSE entryImage
    /\ UNCHANGED <<currentTask, ownerTask, entryImage, stackImage, returnImage,
        active, entryWideState, wideState, bracketDepth, bracketSaved,
        syscallFrameLive, registerMode, marker, entryControl, control,
        asyncWide, asyncControl, userWide, userControl>>

AsyncBlock ==
    /\ phase \in {"syscall", "resumed"} /\ active
    /\ currentTask = ownerTask
    /\ phase' = "async" /\ continuationPublished' = TRUE
    /\ returnValidated' = FALSE
    /\ asyncWide' = wideState /\ asyncControl' = control
    /\ schedulerImage' = entryImage
    /\ UNCHANGED <<currentTask, ownerTask, entryImage, stackImage, returnImage,
        active, entryWideState, wideState, bracketDepth, bracketSaved,
        syscallFrameLive, registerMode, marker, entryControl, control,
        userWide, userControl>>

KernelScratch ==
    /\ phase \in {"syscall", "resumed"} /\ active
    /\ schedulerImage' \in 1..MaxImage
    /\ UNCHANGED <<phase, currentTask, ownerTask, entryImage, stackImage,
        returnImage, active, entryWideState, wideState, bracketDepth,
        bracketSaved, syscallFrameLive, continuationPublished, returnValidated,
        registerMode, marker, entryControl, control, asyncWide, asyncControl,
        userWide, userControl>>

EnterWideSection ==
    /\ phase \in {"syscall", "resumed"} /\ active
    /\ bracketDepth < MaxBracketDepth
    /\ bracketDepth' = bracketDepth + 1
    /\ bracketSaved' = [bracketSaved EXCEPT ![bracketDepth + 1] = wideState]
    /\ UNCHANGED <<phase, currentTask, ownerTask, entryImage, stackImage,
        schedulerImage, returnImage, active, entryWideState, wideState,
        syscallFrameLive, continuationPublished, returnValidated,
        registerMode, marker, entryControl, control, asyncWide, asyncControl,
        userWide, userControl>>
WideScratch ==
    /\ phase \in {"syscall", "resumed"} /\ bracketDepth > 0
    /\ wideState' \in 1..MaxImage
    /\ UNCHANGED <<phase, currentTask, ownerTask, entryImage, stackImage,
        schedulerImage, returnImage, active, entryWideState, bracketDepth,
        bracketSaved, syscallFrameLive, continuationPublished, returnValidated,
        registerMode, marker, entryControl, control, asyncWide, asyncControl,
        userWide, userControl>>
LeaveWideSection ==
    /\ phase \in {"syscall", "resumed"} /\ bracketDepth > 0
    /\ wideState' = bracketSaved[bracketDepth]
    /\ bracketDepth' = bracketDepth - 1
    /\ UNCHANGED <<phase, currentTask, ownerTask, entryImage, stackImage,
        schedulerImage, returnImage, active, entryWideState, bracketSaved,
        syscallFrameLive, continuationPublished, returnValidated,
        registerMode, marker, entryControl, control, asyncWide, asyncControl,
        userWide, userControl>>

ScheduleOther ==
    /\ phase \in {"blocked", "async"} /\ active
    /\ \E task \in 1..MaxTask: /\ task # ownerTask /\ currentTask' = task
    /\ wideState' \in 0..MaxImage /\ control' \in 1..MaxImage
    /\ UNCHANGED <<phase, ownerTask, entryImage, stackImage, schedulerImage,
        returnImage, active, entryWideState, bracketDepth, bracketSaved,
        syscallFrameLive, continuationPublished, returnValidated,
        registerMode, marker, entryControl, asyncWide, asyncControl,
        userWide, userControl>>

ScheduleOwner ==
    /\ phase \in {"blocked", "async"} /\ active
    /\ currentTask' = ownerTask
    /\ wideState' = IF phase = "async" THEN asyncWide
                     ELSE IF registerMode THEN wideState ELSE entryWideState
    /\ control' = IF phase = "async" THEN asyncControl
                   ELSE IF registerMode THEN control ELSE entryControl
    /\ phase' = IF phase = "async" THEN "async-resumed" ELSE "resumed"
    /\ continuationPublished' = FALSE
    /\ UNCHANGED <<ownerTask, entryImage, stackImage, schedulerImage,
        returnImage, active, entryWideState, bracketDepth, bracketSaved,
        syscallFrameLive, returnValidated, registerMode, marker, entryControl,
        asyncWide, asyncControl, userWide, userControl>>

Resume ==
    /\ phase = "async-resumed"
    /\ phase' = "resumed"
    /\ UNCHANGED <<currentTask, ownerTask, entryImage, stackImage, schedulerImage,
        returnImage, active, entryWideState, wideState, bracketDepth,
        bracketSaved, syscallFrameLive, continuationPublished, returnValidated,
        registerMode, marker, entryControl, control, asyncWide, asyncControl,
        userWide, userControl>>

ValidateReturn ==
    /\ phase \in {"syscall", "resumed"} /\ active
    /\ currentTask = ownerTask /\ syscallFrameLive /\ ~continuationPublished
    /\ returnValidated' = TRUE
    /\ UNCHANGED <<phase, currentTask, ownerTask, entryImage, stackImage,
        schedulerImage, returnImage, active, entryWideState, wideState,
        bracketDepth, bracketSaved, syscallFrameLive, continuationPublished,
        registerMode, marker, entryControl, control, asyncWide, asyncControl,
        userWide, userControl>>

Finalize ==
    /\ phase \in {"syscall", "resumed"} /\ active
    /\ currentTask = ownerTask /\ syscallFrameLive /\ ~continuationPublished
    /\ returnValidated /\ bracketDepth = 0
    /\ phase' = "finalized" /\ marker' = 0
    /\ wideState' = IF registerMode THEN 0 ELSE wideState
    /\ control' = entryControl
    /\ stackImage' = IF registerMode THEN 0 ELSE stackImage
    /\ UNCHANGED <<currentTask, ownerTask, entryImage, schedulerImage,
        returnImage, active, entryWideState, bracketDepth, bracketSaved,
        syscallFrameLive, continuationPublished, returnValidated,
        registerMode, entryControl, asyncWide, asyncControl, userWide, userControl>>

Return ==
    /\ phase = "finalized"
    /\ phase' = "returned" /\ returnImage' = stackImage
    /\ active' = FALSE /\ syscallFrameLive' = FALSE
    /\ UNCHANGED <<currentTask, ownerTask, entryImage, stackImage,
        schedulerImage, entryWideState, wideState, bracketDepth, bracketSaved,
        continuationPublished, returnValidated, registerMode, marker,
        entryControl, control, asyncWide, asyncControl, userWide, userControl>>

PublishIret ==
    /\ phase = "finalized"
    /\ phase' = "published-user" /\ continuationPublished' = TRUE
    /\ userWide' = wideState /\ userControl' = control
    /\ UNCHANGED <<currentTask, ownerTask, entryImage, stackImage,
        schedulerImage, returnImage, active, entryWideState, wideState,
        bracketDepth, bracketSaved, syscallFrameLive, returnValidated,
        registerMode, marker, entryControl, control, asyncWide, asyncControl>>

Iret ==
    /\ phase = "published-user"
    /\ phase' = "returned" /\ currentTask' = ownerTask
    /\ returnImage' = stackImage /\ wideState' = userWide /\ control' = userControl
    /\ active' = FALSE /\ syscallFrameLive' = FALSE
    /\ continuationPublished' = FALSE
    /\ UNCHANGED <<ownerTask, entryImage, stackImage, schedulerImage,
        entryWideState, bracketDepth, bracketSaved, returnValidated,
        registerMode, marker, entryControl, asyncWide, asyncControl,
        userWide, userControl>>

Retire ==
    /\ phase \in {"blocked", "async"}
    /\ phase' = "retired" /\ marker' = 0 /\ active' = FALSE
    /\ syscallFrameLive' = FALSE /\ continuationPublished' = FALSE
    /\ UNCHANGED <<currentTask, ownerTask, entryImage, stackImage,
        schedulerImage, returnImage, entryWideState, wideState, bracketDepth,
        bracketSaved, returnValidated, registerMode, entryControl, control,
        asyncWide, asyncControl, userWide, userControl>>
Rebind ==
    /\ phase = "retired" /\ marker = 0
    /\ phase' = "user" /\ currentTask' \in 1..MaxTask /\ ownerTask' = 0
    /\ stackImage' = 0 /\ schedulerImage' = 0 /\ returnImage' = 0
    /\ active' = FALSE /\ wideState' = entryWideState
    /\ bracketDepth' = 0
    /\ bracketSaved' = [level \in 1..MaxBracketDepth |-> 0]
    /\ syscallFrameLive' = FALSE /\ continuationPublished' = FALSE
    /\ returnValidated' = FALSE /\ marker' = 0 /\ control' = entryControl
    /\ asyncWide' = 0 /\ asyncControl' = 0 /\ userWide' = 0 /\ userControl' = 0
    /\ UNCHANGED <<entryImage, entryWideState, registerMode, entryControl>>

Next == Enter \/ Block \/ AsyncBlock \/ KernelScratch \/ EnterWideSection
    \/ WideScratch \/ LeaveWideSection \/ ScheduleOther \/ ScheduleOwner
    \/ Resume \/ ValidateReturn \/ Finalize \/ Return \/ PublishIret \/ Iret \/ Retire \/ Rebind

TypeOK ==
    /\ phase \in Phases /\ currentTask \in 1..MaxTask /\ ownerTask \in 0..MaxTask
    /\ entryImage \in 1..MaxImage /\ stackImage \in 0..MaxImage
    /\ schedulerImage \in 0..MaxImage /\ returnImage \in 0..MaxImage
    /\ active \in BOOLEAN /\ registerMode \in BOOLEAN /\ marker \in 0..MaxTask
    /\ entryWideState \in 1..MaxImage /\ wideState \in 0..MaxImage
    /\ bracketDepth \in 0..MaxBracketDepth
    /\ bracketSaved \in [1..MaxBracketDepth -> 0..MaxImage]
    /\ syscallFrameLive \in BOOLEAN /\ continuationPublished \in BOOLEAN
    /\ returnValidated \in BOOLEAN
    /\ entryControl \in 1..MaxImage /\ control \in 1..MaxImage
    /\ asyncWide \in 0..MaxImage /\ asyncControl \in 0..MaxImage
    /\ userWide \in 0..MaxImage /\ userControl \in 0..MaxImage
ActiveStackImageIsEntryImage == active /\ ~registerMode => stackImage = entryImage
ActiveStackImageHasExactOwner == active => ownerTask \in 1..MaxTask
ReturnRestoresEntryImage == phase = "returned" /\ ~registerMode => returnImage = entryImage
ReturnBelongsToEnteringTask == phase = "returned" => currentTask = ownerTask
ReturnedStackImageIsInactive == phase = "returned" => ~active
SchedulerScratchCannotAuthorizeReturn == phase = "returned" => returnImage = stackImage
UnsavedStateSurvivesUnbracketedKernelCode ==
    ~registerMode /\ bracketDepth = 0 /\ phase \in {"syscall", "async-resumed", "resumed", "finalized", "returned"}
        => wideState = entryWideState
BracketRestoresWideState == phase = "returned" /\ ~registerMode => wideState = entryWideState
ActiveSyscallRetainsItsEntryFrame == active => syscallFrameLive
BlockedContinuationOwnsPublishedFrame == phase \in {"blocked", "async", "published-user"} => continuationPublished
ExecutingContinuationOwnsNoPublishedFrame == phase \in {"syscall", "async-resumed", "resumed", "finalized"} => ~continuationPublished
PublishedContinuationIsNotExecuting == continuationPublished => phase \in {"blocked", "async", "published-user"}
ReturnConsumesOnlyPostResumeValidatedFrame == phase = "returned" =>
    /\ returnValidated /\ ~syscallFrameLive /\ ~continuationPublished
RegisterMarkerHasExactLiveOwner == marker # 0 =>
    /\ registerMode /\ active /\ marker = ownerTask
    /\ phase \in {"syscall", "blocked", "async", "async-resumed", "resumed"}
BothReturnsFinalizeRegisterData == registerMode /\ phase \in {"finalized", "published-user", "returned"} =>
    /\ marker = 0 /\ wideState = 0 /\ stackImage = 0 /\ control = entryControl
PublishedUserCaptureIsSanitized == registerMode /\ phase = "published-user" =>
    /\ userWide = 0 /\ userControl = entryControl
AsyncResumeRestoresOpenBracket == phase = "async-resumed" =>
    /\ wideState = asyncWide /\ control = asyncControl
RetiredAndUnenteredHaveNoMarker == phase \in {"retired", "user", "returned"} => marker = 0
Spec == Init /\ [][Next]_vars
===============================================================================
