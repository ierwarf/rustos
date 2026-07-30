----------------------------- MODULE SchedulerWakeup -----------------------------
EXTENDS Naturals

(*******************************************************************************
Models the scheduler half of a bounded IPC wait: arm, arm a deadline timer,
recheck/commit, wake, timer expiry, and task retirement.

Concrete owners:
  * kernel/ps/src/multitask/scheduler.rs
  * kernel/ps/src/multitask/current.rs
  * kernel/ps/src/multitask/irq.rs

`arm_block_current_task` records an armed wake without removing the current
task from the runnable path. `wake_task_slot` clears that arm before a later
`commit_block_current_task` can make the task blocked.  The timer IRQ runs its
monotonic deadline processing before the scheduler chooses a subsequent task. This
model makes the arm identity explicit: a wake invalidates that identity, so a
later block must come from a newly armed epoch rather than a lost wakeup.

Linearization points: arm records an epoch; wake/expiry invalidates that epoch;
commit changes the current task to Blocked and transfers CPU ownership in the
same interrupt-excluded software-schedule transition; a deadline record remains
owned until the resumed task acknowledges it; retirement clears all scheduler
and timer authority for the task. It does not assert fairness or an eventual
CPU slice, which require a different temporal/fairness model.
*******************************************************************************)

CONSTANTS Tasks, MaxTick, MaxArmEpoch

NoTask == 0
NoTimer == 0

Ready == "ready"
Armed == "armed"
Blocked == "blocked"
Retired == "retired"

VARIABLES now,
          current,
          taskState,
          timerDeadline,
          armEpoch,
          lastWakeEpoch,
          blockedEpoch,
          resumePending,
          framePublished

vars == <<now, current, taskState, timerDeadline, armEpoch, lastWakeEpoch,
          blockedEpoch, resumePending, framePublished>>

Init ==
    /\ now = 0
    /\ current = NoTask
    /\ taskState = [t \in Tasks |-> Ready]
    /\ timerDeadline = [t \in Tasks |-> NoTimer]
    /\ armEpoch = [t \in Tasks |-> 0]
    /\ lastWakeEpoch = [t \in Tasks |-> 0]
    /\ blockedEpoch = [t \in Tasks |-> 0]
    /\ resumePending = [t \in Tasks |-> FALSE]
    /\ framePublished = [t \in Tasks |-> TRUE]

Dispatch(task) ==
    /\ task \in Tasks
    /\ current = NoTask
    /\ taskState[task] = Ready
    /\ current' = task
    /\ framePublished' = [framePublished EXCEPT ![task] = FALSE]
    /\ UNCHANGED <<now, taskState, timerDeadline, armEpoch, lastWakeEpoch,
                  blockedEpoch, resumePending>>

ReleaseCurrent(task) ==
    /\ task \in Tasks
    /\ current = task
    /\ taskState[task] = Ready
    /\ current' = NoTask
    /\ framePublished' = [framePublished EXCEPT ![task] = TRUE]
    /\ UNCHANGED <<now, taskState, timerDeadline, armEpoch, lastWakeEpoch,
                  blockedEpoch, resumePending>>

(*******************************************************************************
The first half of wait_for_reply_with_deadline. Armed remains schedulable: it
is still the current task until commit, which is why a concurrent wake must
clear the arm rather than queue a second runnable copy.
*******************************************************************************)
ArmCurrentBlock(task) ==
    /\ task \in Tasks
    /\ current = task
    /\ taskState[task] = Ready
    /\ timerDeadline[task] = NoTimer
    /\ now < MaxTick
    /\ armEpoch[task] < MaxArmEpoch
    /\ taskState' = [taskState EXCEPT ![task] = Armed]
    /\ armEpoch' = [armEpoch EXCEPT ![task] = armEpoch[task] + 1]
    /\ UNCHANGED <<now, current, timerDeadline, lastWakeEpoch, blockedEpoch>>
    /\ UNCHANGED <<resumePending, framePublished>>

ArmDeadlineTimer(task) ==
    /\ task \in Tasks
    /\ current = task
    /\ taskState[task] = Armed
    /\ timerDeadline[task] = NoTimer
    /\ now < MaxTick
    /\ timerDeadline' = [timerDeadline EXCEPT ![task] = now + 1]
    /\ UNCHANGED <<now, current, taskState, armEpoch, lastWakeEpoch,
                  blockedEpoch, resumePending, framePublished>>

(*******************************************************************************
The recheck has observed no wake and no completed reply. A committed block is
legal only while the exact armed epoch and its timer are both still present.
*******************************************************************************)
CommitCurrentBlock(task) ==
    /\ task \in Tasks
    /\ current = task
    /\ taskState[task] = Armed
    /\ timerDeadline[task] > now
    /\ armEpoch[task] > lastWakeEpoch[task]
    /\ taskState' = [taskState EXCEPT ![task] = Blocked]
    /\ current' = NoTask
    /\ blockedEpoch' = [blockedEpoch EXCEPT ![task] = armEpoch[task]]
    /\ framePublished' = [framePublished EXCEPT ![task] = TRUE]
    /\ UNCHANGED <<now, timerDeadline, armEpoch, lastWakeEpoch, resumePending>>

(*******************************************************************************
An IPC reply, cancellation, or peer teardown invokes this path. In particular,
when it races before commit it leaves the task Ready while it is still current;
the subsequent commit guard is false and the caller rechecks instead of
sleeping through the already-delivered wake.  A running or armed task has no
published resumable frame: its previous frame was consumed by Dispatch and may
already be ordinary stack storage.  Wake therefore validates/preserves a frame
only for a Blocked task and treats an Armed wake as a token transition.
*******************************************************************************)
WakeTask(task) ==
    /\ task \in Tasks
    /\ taskState[task] \in {Armed, Blocked}
    /\ taskState' = [taskState EXCEPT ![task] = Ready]
    /\ lastWakeEpoch' =
        [lastWakeEpoch EXCEPT
            ![task] = IF taskState[task] = Armed
                     THEN armEpoch[task]
                     ELSE blockedEpoch[task]]
    /\ resumePending' =
        [resumePending EXCEPT
            ![task] = resumePending[task] \/ taskState[task] = Blocked]
    /\ UNCHANGED framePublished
    /\ UNCHANGED <<now, current, timerDeadline, armEpoch, blockedEpoch>>

(*******************************************************************************
A clockevent wake is notification, not ownership transfer. The deadline stays
published through scheduler wake and kernel cleanup. Only the resumed task can
complete the wait and acknowledge the timer, so a scheduler-side lost
transition or a stalled cleanup cannot delete the only recovery authority.
*******************************************************************************)
CompleteWait(task) ==
    /\ task \in Tasks
    /\ current = task
    /\ taskState[task] = Ready
    /\ timerDeadline[task] # NoTimer
    /\ timerDeadline' = [timerDeadline EXCEPT ![task] = NoTimer]
    /\ resumePending' = [resumePending EXCEPT ![task] = FALSE]
    /\ UNCHANGED <<now, current, taskState, armEpoch, lastWakeEpoch,
                  blockedEpoch, framePublished>>

CancelCurrentArm(task) ==
    /\ task \in Tasks
    /\ current = task
    /\ taskState[task] = Armed
    /\ taskState' = [taskState EXCEPT ![task] = Ready]
    /\ timerDeadline' = [timerDeadline EXCEPT ![task] = NoTimer]
    /\ lastWakeEpoch' = [lastWakeEpoch EXCEPT ![task] = armEpoch[task]]
    /\ resumePending' = [resumePending EXCEPT ![task] = FALSE]
    /\ UNCHANGED <<now, current, armEpoch, blockedEpoch>>
    /\ UNCHANGED framePublished

(*******************************************************************************
irq.rs invokes the PIT clockevent's monotonic deadline service before scheduler
selection. Due waits are therefore made Ready in the same Tick transition,
before Dispatch can be enabled from the resulting state.
*******************************************************************************)
Tick ==
    LET DueTasks ==
        {t \in Tasks : taskState[t] \in {Ready, Armed, Blocked}
                      /\ timerDeadline[t] # NoTimer
                      /\ timerDeadline[t] <= now + 1} IN
    /\ now < MaxTick
    /\ \A task \in Tasks :
        taskState[task] = Armed => timerDeadline[task] # NoTimer
    /\ now' = now + 1
    /\ taskState' =
        [t \in Tasks |-> IF t \in DueTasks THEN Ready ELSE taskState[t]]
    /\ UNCHANGED timerDeadline
    /\ lastWakeEpoch' =
        [t \in Tasks |->
            IF t \in DueTasks /\ taskState[t] \in {Armed, Blocked}
            THEN IF taskState[t] = Armed THEN armEpoch[t] ELSE blockedEpoch[t]
            ELSE lastWakeEpoch[t]]
    /\ resumePending' =
        [t \in Tasks |->
            resumePending[t] \/ (t \in DueTasks /\ taskState[t] = Blocked)]
    /\ UNCHANGED framePublished
    /\ UNCHANGED <<current, armEpoch, blockedEpoch>>

RetireTask(task) ==
    /\ task \in Tasks
    /\ taskState[task] # Retired
    /\ taskState' = [taskState EXCEPT ![task] = Retired]
    /\ timerDeadline' = [timerDeadline EXCEPT ![task] = NoTimer]
    /\ resumePending' = [resumePending EXCEPT ![task] = FALSE]
    /\ current' = IF current = task THEN NoTask ELSE current
    /\ framePublished' = [framePublished EXCEPT ![task] = FALSE]
    /\ UNCHANGED <<now, armEpoch, lastWakeEpoch, blockedEpoch>>

Next ==
    \/ \E task \in Tasks : Dispatch(task)
    \/ \E task \in Tasks : ReleaseCurrent(task)
    \/ \E task \in Tasks : ArmCurrentBlock(task)
    \/ \E task \in Tasks : ArmDeadlineTimer(task)
    \/ \E task \in Tasks : CommitCurrentBlock(task)
    \/ \E task \in Tasks : WakeTask(task)
    \/ \E task \in Tasks : CompleteWait(task)
    \/ \E task \in Tasks : CancelCurrentArm(task)
    \/ Tick
    \/ \E task \in Tasks : RetireTask(task)

TypeOK ==
    /\ Tasks \subseteq Nat
    /\ NoTask \notin Tasks
    /\ MaxTick \in Nat
    /\ MaxArmEpoch \in Nat
    /\ now \in 0..MaxTick
    /\ current \in Tasks \cup {NoTask}
    /\ taskState \in [Tasks -> {Ready, Armed, Blocked, Retired}]
    /\ timerDeadline \in [Tasks -> 0..MaxTick]
    /\ armEpoch \in [Tasks -> 0..MaxArmEpoch]
    /\ lastWakeEpoch \in [Tasks -> 0..MaxArmEpoch]
    /\ blockedEpoch \in [Tasks -> 0..MaxArmEpoch]
    /\ resumePending \in [Tasks -> BOOLEAN]
    /\ framePublished \in [Tasks -> BOOLEAN]

CurrentTaskIsRunnable ==
    current # NoTask => taskState[current] \in {Ready, Armed}

AnArmedTaskStillOwnsTheCpu ==
    \A task \in Tasks : taskState[task] = Armed => current = task

BlockedTaskOwnsNoCpu ==
    \A task \in Tasks : taskState[task] = Blocked => current # task

CurrentTaskOwnsNoPublishedFrame ==
    current # NoTask => ~framePublished[current]

NonCurrentReadyTaskOwnsPublishedFrame ==
    \A task \in Tasks :
        taskState[task] = Ready /\ current # task => framePublished[task]

BlockedTaskOwnsPublishedFrame ==
    \A task \in Tasks : taskState[task] = Blocked => framePublished[task]

ArmedTaskOwnsNoPublishedFrame ==
    \A task \in Tasks : taskState[task] = Armed => ~framePublished[task]

TimerHasOneLiveOwner ==
    \A task \in Tasks :
        timerDeadline[task] # NoTimer =>
            /\ taskState[task] \in {Ready, Armed, Blocked}
            /\ (taskState[task] = Ready \/ timerDeadline[task] > now)

BlockedTaskHasAnUnexpiredCommittedDeadline ==
    \A task \in Tasks :
        taskState[task] = Blocked =>
            /\ timerDeadline[task] # NoTimer
            /\ timerDeadline[task] > now
            /\ blockedEpoch[task] = armEpoch[task]

WakeInvalidatesTheCurrentArmEpoch ==
    \A task \in Tasks :
        taskState[task] = Armed => armEpoch[task] > lastWakeEpoch[task]

NoWakeCanCommitTheSameArmEpoch ==
    \A task \in Tasks :
        taskState[task] = Blocked => blockedEpoch[task] > lastWakeEpoch[task]

NoExpiredSleeperSurvivesTimerInterrupt ==
    \A task \in Tasks :
        taskState[task] = Blocked => timerDeadline[task] > now

RetiredTaskHasNoSchedulingOrTimerAuthority ==
    \A task \in Tasks :
        taskState[task] = Retired =>
            /\ current # task
            /\ timerDeadline[task] = NoTimer

PendingResumeRetainsRecoveryAuthority ==
    \A task \in Tasks :
        resumePending[task] =>
            /\ taskState[task] = Ready
            /\ timerDeadline[task] # NoTimer

TimerArmedWaitEventuallyReleases ==
    \A task \in Tasks:
        timerDeadline[task] # NoTimer ~>
            (timerDeadline[task] = NoTimer \/ taskState[task] = Retired)

\* These are the explicit clock, dispatch, and bounded kernel-resume progress
\* assumptions. Safety shows which owner may mutate each stage; fairness proves
\* a finite deadline cannot be acknowledged merely because wake was published:
\* the task must be dispatched and complete its kernel wait cleanup.
Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(Tick)
    /\ \A task \in Tasks : WF_vars(ArmDeadlineTimer(task))
    /\ \A task \in Tasks : WF_vars(ReleaseCurrent(task))
    /\ \A task \in Tasks : SF_vars(Dispatch(task))
    /\ \A task \in Tasks : SF_vars(CompleteWait(task))

=============================================================================
