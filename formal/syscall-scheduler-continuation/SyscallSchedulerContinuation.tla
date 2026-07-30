---------------- MODULE SyscallSchedulerContinuation ----------------
EXTENDS Naturals

(*******************************************************************************
Composes the syscall-entry frame lifetime with the scheduler's atomic
check-arm-commit wake protocol.

The syscall frame remains live on the exact owner's kernel stack.  The
scheduler frame is different authority: it is consumed while the owner runs,
published atomically when blocking transfers CPU ownership, remains published
through wake, and is consumed by dispatch.  A wake that races an Armed owner
changes only the wake epoch; it cannot validate or publish the previously
consumed scheduler frame.  SYSRET validation follows the last resume.
*******************************************************************************)

CONSTANTS Tasks, MaxEpoch

NoTask == 0
Absent == "absent"
Live == "live"
Consumed == "consumed"
Published == "published"

Phases ==
    {"user", "syscall", "armed", "blocked", "ready", "resumed",
     "validated", "returned", "retired"}

VARIABLES phase,
          owner,
          current,
          syscallFrame,
          continuationFrame,
          armEpoch,
          lastWakeEpoch,
          returnValidated

vars == <<phase, owner, current, syscallFrame, continuationFrame,
          armEpoch, lastWakeEpoch, returnValidated>>

Init ==
    /\ owner \in Tasks
    /\ phase = "user"
    /\ current = owner
    /\ syscallFrame = Absent
    /\ continuationFrame = Consumed
    /\ armEpoch = 0
    /\ lastWakeEpoch = 0
    /\ returnValidated = FALSE

EnterSyscall ==
    /\ phase = "user"
    /\ current = owner
    /\ phase' = "syscall"
    /\ syscallFrame' = Live
    /\ returnValidated' = FALSE
    /\ UNCHANGED <<owner, current, continuationFrame, armEpoch, lastWakeEpoch>>

ArmBlock ==
    /\ phase = "syscall"
    /\ current = owner
    /\ continuationFrame = Consumed
    /\ armEpoch < MaxEpoch
    /\ phase' = "armed"
    /\ armEpoch' = armEpoch + 1
    /\ UNCHANGED <<owner, current, syscallFrame, continuationFrame,
                   lastWakeEpoch, returnValidated>>

RacedWake ==
    /\ phase = "armed"
    /\ current = owner
    /\ continuationFrame = Consumed
    /\ phase' = "syscall"
    /\ lastWakeEpoch' = armEpoch
    /\ UNCHANGED <<owner, current, syscallFrame, continuationFrame,
                   armEpoch, returnValidated>>

CommitBlock ==
    /\ phase = "armed"
    /\ current = owner
    /\ armEpoch > lastWakeEpoch
    /\ phase' = "blocked"
    /\ current' = NoTask
    /\ continuationFrame' = Published
    /\ returnValidated' = FALSE
    /\ UNCHANGED <<owner, syscallFrame, armEpoch, lastWakeEpoch>>

RunOther ==
    /\ phase \in {"blocked", "ready"}
    /\ current = NoTask
    /\ \E task \in Tasks:
        /\ task # owner
        /\ current' = task
    /\ UNCHANGED <<phase, owner, syscallFrame, continuationFrame,
                   armEpoch, lastWakeEpoch, returnValidated>>

ReleaseOther ==
    /\ phase \in {"blocked", "ready"}
    /\ current \in Tasks \ {owner}
    /\ current' = NoTask
    /\ UNCHANGED <<phase, owner, syscallFrame, continuationFrame,
                   armEpoch, lastWakeEpoch, returnValidated>>

WakeBlocked ==
    /\ phase = "blocked"
    /\ continuationFrame = Published
    /\ phase' = "ready"
    /\ lastWakeEpoch' = armEpoch
    /\ UNCHANGED <<owner, current, syscallFrame, continuationFrame,
                   armEpoch, returnValidated>>

DispatchOwner ==
    /\ phase = "ready"
    /\ current = NoTask
    /\ continuationFrame = Published
    /\ phase' = "resumed"
    /\ current' = owner
    /\ continuationFrame' = Consumed
    /\ returnValidated' = FALSE
    /\ UNCHANGED <<owner, syscallFrame, armEpoch, lastWakeEpoch>>

ValidateReturn ==
    /\ phase \in {"syscall", "resumed"}
    /\ current = owner
    /\ syscallFrame = Live
    /\ continuationFrame = Consumed
    /\ phase' = "validated"
    /\ returnValidated' = TRUE
    /\ UNCHANGED <<owner, current, syscallFrame, continuationFrame,
                   armEpoch, lastWakeEpoch>>

ReturnToUser ==
    /\ phase = "validated"
    /\ current = owner
    /\ syscallFrame = Live
    /\ continuationFrame = Consumed
    /\ returnValidated
    /\ phase' = "returned"
    /\ syscallFrame' = Absent
    /\ UNCHANGED <<owner, current, continuationFrame, armEpoch,
                   lastWakeEpoch, returnValidated>>

Retire ==
    /\ phase \notin {"returned", "retired"}
    /\ phase' = "retired"
    /\ current' = IF current = owner THEN NoTask ELSE current
    /\ syscallFrame' = Absent
    /\ continuationFrame' = Absent
    /\ returnValidated' = FALSE
    /\ UNCHANGED <<owner, armEpoch, lastWakeEpoch>>

TerminalStutter ==
    /\ phase \in {"returned", "retired"}
    /\ UNCHANGED vars

Next ==
    \/ EnterSyscall
    \/ ArmBlock
    \/ RacedWake
    \/ CommitBlock
    \/ RunOther
    \/ ReleaseOther
    \/ WakeBlocked
    \/ DispatchOwner
    \/ ValidateReturn
    \/ ReturnToUser
    \/ Retire
    \/ TerminalStutter

TypeOK ==
    /\ phase \in Phases
    /\ owner \in Tasks
    /\ current \in Tasks \cup {NoTask}
    /\ syscallFrame \in {Absent, Live}
    /\ continuationFrame \in {Absent, Consumed, Published}
    /\ armEpoch \in 0..MaxEpoch
    /\ lastWakeEpoch \in 0..MaxEpoch
    /\ returnValidated \in BOOLEAN

LiveSyscallFrameHasExactPhase ==
    (syscallFrame = Live) =
        (phase \in {"syscall", "armed", "blocked", "ready", "resumed",
                    "validated"})

PublishedContinuationIsNeverExecuting ==
    continuationFrame = Published =>
        /\ phase \in {"blocked", "ready"}
        /\ current # owner

RunningOwnerHasConsumedContinuation ==
    current = owner => continuationFrame = Consumed

ArmedWakeUsesNoPublishedFrame ==
    phase = "armed" =>
        /\ current = owner
        /\ continuationFrame = Consumed

BlockedAndReadyRetainOneContinuation ==
    phase \in {"blocked", "ready"} => continuationFrame = Published

ReturnedOnlyAfterPostResumeValidation ==
    phase = "returned" =>
        /\ returnValidated
        /\ syscallFrame = Absent
        /\ continuationFrame = Consumed
        /\ current = owner

RetiredOwnsNoFrame ==
    phase = "retired" =>
        /\ syscallFrame = Absent
        /\ continuationFrame = Absent
        /\ current # owner

Spec == Init /\ [][Next]_vars

=============================================================================
