-------------------------- MODULE ClocksourceDeadline --------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models RustOS monotonic time and finite sleep after the KVM UI soak exposed two
implementation bugs:

  * counting CMOS RTC periodic interrupts made guest time advance only about
    thirteen seconds during a thirty-second host run;
  * resolving a sleeper through current_user_snapshot re-entered the
    process-table lock from syscall context and deadlocked before waiter arm.
  * clock_gettime/nanosleep admission still took the shared process-state lock
    (and could synchronously call syscalld) before any timer authority existed,
    so one contended lookup turned a bounded userspace sleep into an infinite
    pre-arm stall;
  * deadline wake used monotonic time while the outer sleep loop still tested
    the disabled periodic-RTC counter, so an expired finite sleep spun forever.

Concrete owners:
  * kernel/hal/src/arch/{acpi.rs,clock.rs,rtc.rs}
  * kernel/hal/src/hooks.rs
  * kernel/ps/src/multitask/{current.rs,scheduler.rs,irq.rs}

Elapsed time is read from a validated invariant-TSC/HPET clocksource. PIT is a
clockevent only: one delayed event services every absolute deadline at or below
the current source time. Sleeper identity is the scheduler task id and remains
independent from a process-table lock already held by the syscall path.
The fixed clock/timespec ABI envelope is admitted locally without process-state
or policy-service participation.
Deadline construction, expiry, and the caller's completion condition all use
`sourceTime`; no second counter exists in the specification.
*******************************************************************************)

CONSTANTS Tasks, MaxTime, MaxJump, MaxArmEpoch

NoTask == 0
NoDeadline == 0

Uninitialized == "uninitialized"
InvariantTsc == "invariant-tsc"
Hpet == "hpet"

Ready == "ready"
Armed == "armed"
Blocked == "blocked"
Retired == "retired"

VARIABLES sourceKind,
          sourceCalibrated,
          sourceTime,
          servicedTime,
          deliveredEvents,
          taskState,
          deadline,
          deadlineOwner,
          localAdmission,
          processLockHeld,
          armEpoch,
          lastWakeEpoch

vars == <<sourceKind, sourceCalibrated, sourceTime, servicedTime,
          deliveredEvents, taskState, deadline, deadlineOwner,
          localAdmission, processLockHeld, armEpoch, lastWakeEpoch>>

SourceValid ==
    /\ sourceKind \in {InvariantTsc, Hpet}
    /\ sourceCalibrated

Init ==
    /\ sourceKind = Uninitialized
    /\ sourceCalibrated = FALSE
    /\ sourceTime = 0
    /\ servicedTime = 0
    /\ deliveredEvents = 0
    /\ taskState = [t \in Tasks |-> Ready]
    /\ deadline = [t \in Tasks |-> NoDeadline]
    /\ deadlineOwner = [t \in Tasks |-> NoTask]
    /\ localAdmission = {}
    /\ processLockHeld = {}
    /\ armEpoch = [t \in Tasks |-> 0]
    /\ lastWakeEpoch = [t \in Tasks |-> 0]

SelectInvariantTsc ==
    /\ sourceKind = Uninitialized
    /\ sourceKind' = InvariantTsc
    /\ sourceCalibrated' = TRUE
    /\ UNCHANGED <<sourceTime, servicedTime, deliveredEvents, taskState,
                  deadline, deadlineOwner, localAdmission, processLockHeld, armEpoch,
                  lastWakeEpoch>>

SelectHpet ==
    /\ sourceKind = Uninitialized
    /\ sourceKind' = Hpet
    /\ sourceCalibrated' = TRUE
    /\ UNCHANGED <<sourceTime, servicedTime, deliveredEvents, taskState,
                  deadline, deadlineOwner, localAdmission, processLockHeld, armEpoch,
                  lastWakeEpoch>>

EnterSyscall(task) ==
    /\ task \in Tasks
    /\ taskState[task] = Ready
    /\ task \notin processLockHeld
    /\ processLockHeld' = processLockHeld \cup {task}
    /\ UNCHANGED <<sourceKind, sourceCalibrated, sourceTime, servicedTime,
                  deliveredEvents, taskState, deadline, deadlineOwner,
                  localAdmission, armEpoch, lastWakeEpoch>>

ExitSyscall(task) ==
    /\ task \in processLockHeld
    /\ processLockHeld' = processLockHeld \ {task}
    /\ UNCHANGED <<sourceKind, sourceCalibrated, sourceTime, servicedTime,
                  deliveredEvents, taskState, deadline, deadlineOwner,
                  localAdmission, armEpoch, lastWakeEpoch>>

AdmitDeadlineLocally(task) ==
    /\ SourceValid
    /\ task \in Tasks
    /\ taskState[task] = Ready
    /\ deadline[task] = NoDeadline
    /\ localAdmission' = localAdmission \cup {task}
    /\ UNCHANGED <<sourceKind, sourceCalibrated, sourceTime, servicedTime,
                  deliveredEvents, taskState, deadline, deadlineOwner,
                  processLockHeld, armEpoch, lastWakeEpoch>>

(*******************************************************************************
Arm resolves the exact scheduler task identity directly. It is intentionally
enabled while processLockHeld contains task: attempting to reacquire that lock
is not part of this transition.
*******************************************************************************)
ArmSleep(task) ==
    /\ SourceValid
    /\ task \in Tasks
    /\ taskState[task] = Ready
    /\ task \in localAdmission
    /\ deadline[task] = NoDeadline
    /\ sourceTime < MaxTime
    /\ armEpoch[task] < MaxArmEpoch
    /\ taskState' = [taskState EXCEPT ![task] = Armed]
    /\ deadline' = [deadline EXCEPT ![task] = sourceTime + 1]
    /\ deadlineOwner' = [deadlineOwner EXCEPT ![task] = task]
    /\ armEpoch' = [armEpoch EXCEPT ![task] = armEpoch[task] + 1]
    /\ UNCHANGED <<sourceKind, sourceCalibrated, sourceTime, servicedTime,
                  deliveredEvents, localAdmission, processLockHeld, lastWakeEpoch>>

CommitSleep(task) ==
    /\ task \in Tasks
    /\ taskState[task] = Armed
    /\ deadline[task] > sourceTime
    /\ armEpoch[task] > lastWakeEpoch[task]
    /\ taskState' = [taskState EXCEPT ![task] = Blocked]
    /\ UNCHANGED <<sourceKind, sourceCalibrated, sourceTime, servicedTime,
                  deliveredEvents, deadline, deadlineOwner, processLockHeld,
                  localAdmission, armEpoch, lastWakeEpoch>>

CancelExpiredArm(task) ==
    /\ task \in Tasks
    /\ taskState[task] = Armed
    /\ deadline[task] <= sourceTime
    /\ taskState' = [taskState EXCEPT ![task] = Ready]
    /\ deadline' = [deadline EXCEPT ![task] = NoDeadline]
    /\ deadlineOwner' = [deadlineOwner EXCEPT ![task] = NoTask]
    /\ localAdmission' = localAdmission \ {task}
    /\ lastWakeEpoch' = [lastWakeEpoch EXCEPT ![task] = armEpoch[task]]
    /\ UNCHANGED <<sourceKind, sourceCalibrated, sourceTime, servicedTime,
                  deliveredEvents, processLockHeld, armEpoch>>

WakeTask(task) ==
    /\ task \in Tasks
    /\ taskState[task] \in {Armed, Blocked}
    /\ taskState' = [taskState EXCEPT ![task] = Ready]
    /\ deadline' = [deadline EXCEPT ![task] = NoDeadline]
    /\ deadlineOwner' = [deadlineOwner EXCEPT ![task] = NoTask]
    /\ localAdmission' = localAdmission \ {task}
    /\ lastWakeEpoch' = [lastWakeEpoch EXCEPT ![task] = armEpoch[task]]
    /\ UNCHANGED <<sourceKind, sourceCalibrated, sourceTime, servicedTime,
                  deliveredEvents, processLockHeld, armEpoch>>

(*******************************************************************************
Clocksource time may jump by more than one unit while the vCPU is descheduled.
This does not fabricate several delivered interrupts and does not regress time.
*******************************************************************************)
AdvanceSource(step) ==
    /\ SourceValid
    /\ step \in 1..MaxJump
    /\ sourceTime + step <= MaxTime
    /\ sourceTime' = sourceTime + step
    /\ UNCHANGED <<sourceKind, sourceCalibrated, servicedTime,
                  deliveredEvents, taskState, deadline, deadlineOwner,
                  localAdmission, processLockHeld, armEpoch, lastWakeEpoch>>

(*******************************************************************************
One PIT clockevent catches up every due absolute deadline, including deadlines
crossed by a multi-unit source jump. The event count is observability only and
never contributes to sourceTime.
*******************************************************************************)
DeliverClockEvent ==
    LET Due == {t \in Tasks :
                    taskState[t] \in {Armed, Blocked}
                    /\ deadline[t] # NoDeadline
                    /\ deadline[t] <= sourceTime} IN
    /\ SourceValid
    /\ servicedTime < sourceTime
    /\ servicedTime' = sourceTime
    /\ deliveredEvents' = deliveredEvents + 1
    /\ taskState' =
        [t \in Tasks |-> IF t \in Due THEN Ready ELSE taskState[t]]
    /\ deadline' =
        [t \in Tasks |-> IF t \in Due THEN NoDeadline ELSE deadline[t]]
    /\ deadlineOwner' =
        [t \in Tasks |-> IF t \in Due THEN NoTask ELSE deadlineOwner[t]]
    /\ lastWakeEpoch' =
        [t \in Tasks |-> IF t \in Due THEN armEpoch[t] ELSE lastWakeEpoch[t]]
    /\ localAdmission' = localAdmission \ Due
    /\ UNCHANGED <<sourceKind, sourceCalibrated, sourceTime,
                  processLockHeld, armEpoch>>

RetireTask(task) ==
    /\ task \in Tasks
    /\ taskState[task] # Retired
    /\ taskState' = [taskState EXCEPT ![task] = Retired]
    /\ deadline' = [deadline EXCEPT ![task] = NoDeadline]
    /\ deadlineOwner' = [deadlineOwner EXCEPT ![task] = NoTask]
    /\ localAdmission' = localAdmission \ {task}
    /\ processLockHeld' = processLockHeld \ {task}
    /\ UNCHANGED <<sourceKind, sourceCalibrated, sourceTime, servicedTime,
                  deliveredEvents, armEpoch, lastWakeEpoch>>

Next ==
    \/ SelectInvariantTsc
    \/ SelectHpet
    \/ \E task \in Tasks : EnterSyscall(task)
    \/ \E task \in Tasks : ExitSyscall(task)
    \/ \E task \in Tasks : AdmitDeadlineLocally(task)
    \/ \E task \in Tasks : ArmSleep(task)
    \/ \E task \in Tasks : CommitSleep(task)
    \/ \E task \in Tasks : CancelExpiredArm(task)
    \/ \E task \in Tasks : WakeTask(task)
    \/ \E step \in 1..MaxJump : AdvanceSource(step)
    \/ DeliverClockEvent
    \/ \E task \in Tasks : RetireTask(task)

TypeOK ==
    /\ Tasks \subseteq Nat
    /\ NoTask \notin Tasks
    /\ MaxTime \in Nat
    /\ MaxJump \in Nat
    /\ MaxArmEpoch \in Nat
    /\ sourceKind \in {Uninitialized, InvariantTsc, Hpet}
    /\ sourceCalibrated \in BOOLEAN
    /\ sourceTime \in 0..MaxTime
    /\ servicedTime \in 0..MaxTime
    /\ servicedTime <= sourceTime
    /\ deliveredEvents \in Nat
    /\ taskState \in [Tasks -> {Ready, Armed, Blocked, Retired}]
    /\ deadline \in [Tasks -> 0..MaxTime]
    /\ deadlineOwner \in [Tasks -> Tasks \cup {NoTask}]
    /\ localAdmission \subseteq Tasks
    /\ processLockHeld \subseteq Tasks
    /\ armEpoch \in [Tasks -> 0..MaxArmEpoch]
    /\ lastWakeEpoch \in [Tasks -> 0..MaxArmEpoch]

SelectedSourceIsValidated ==
    sourceKind # Uninitialized => SourceValid

UnvalidatedClockOwnsNoDeadline ==
    ~SourceValid => \A task \in Tasks : deadline[task] = NoDeadline

DeadlineUsesExactSchedulerIdentity ==
    \A task \in Tasks :
        deadline[task] # NoDeadline => deadlineOwner[task] = task

ProcessLockNeverOwnsDeadlineIdentity ==
    \A task \in processLockHeld :
        deadline[task] # NoDeadline => deadlineOwner[task] = task

ArmedOrBlockedOwnsExactlyOneDeadline ==
    \A task \in Tasks :
        taskState[task] \in {Armed, Blocked} =>
            /\ deadline[task] # NoDeadline
            /\ deadlineOwner[task] = task
            /\ task \in localAdmission

NoServicedDeadlineRemainsAsleep ==
    \A task \in Tasks :
        taskState[task] \in {Armed, Blocked} => deadline[task] > servicedTime

WakeInvalidatesArmEpoch ==
    \A task \in Tasks :
        taskState[task] \in {Armed, Blocked} => armEpoch[task] > lastWakeEpoch[task]

RetiredTaskHasNoTimerAuthority ==
    \A task \in Tasks :
        taskState[task] = Retired =>
            /\ deadline[task] = NoDeadline
            /\ deadlineOwner[task] = NoTask
            /\ task \notin localAdmission
            /\ task \notin processLockHeld

BlockedSleepEventuallyReleases ==
    \A task \in Tasks :
        taskState[task] = Blocked ~> taskState[task] \in {Ready, Retired}

Spec ==
    Init
    /\ [][Next]_vars
    /\ WF_vars(DeliverClockEvent)
    /\ \A step \in 1..MaxJump : WF_vars(AdvanceSource(step))

=============================================================================
