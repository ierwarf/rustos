------------------------ MODULE TaskAffinityLifecycle ------------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Models the effective CPU-affinity lifecycle for one live task in the fixed
single-group RustOS SMP envelope.

Linux sched_setaffinity changes one thread mask. Windows process affinity
changes the containing process mask and every extant thread mask atomically;
Windows thread affinity must remain a subset of the process mask. A successful
change returns or preserves the previous mask as required by the public ABI.
If the running CPU is no longer allowed, the task enters a bounded migration
obligation and cannot execute user work again until an allowed CPU owns it.
Fork/clone inherits the effective parent mask and exec preserves it.

Concrete owners:
  * kernel/ps/src/multitask/scheduler/affinity.rs
  * kernel/ps/src/multitask/scheduler.rs
  * kernel/compat/src/user/syscall/linux/syscalld_ops.rs
  * kernel/compat/src/user/syscall/windows/dispatch.rs
  * services/syscalld/src/affinity_policy.rs
  * compat/windows/services/winsys/{ntdll/syscall.c,kernelbase/exports.c}
***************************************************************************)

CONSTANTS OnlineCpus, InitialCpu, RequestedMask, InvalidMask, MaxExecGeneration

VARIABLES processMask, taskMask, previousMask, runningCpu, migrationPending,
          childMask, publishedParentMask, childPublished, execGeneration,
          observedCpu, phase

vars == <<processMask, taskMask, previousMask, runningCpu, migrationPending,
          childMask, publishedParentMask, childPublished, execGeneration,
          observedCpu, phase>>

Init ==
    /\ processMask = OnlineCpus
    /\ taskMask = OnlineCpus
    /\ previousMask = {}
    /\ runningCpu = InitialCpu
    /\ migrationPending = FALSE
    /\ childMask = {}
    /\ publishedParentMask = {}
    /\ childPublished = FALSE
    /\ execGeneration = 1
    /\ observedCpu = InitialCpu
    /\ phase = "ready"

MaskValid(mask) ==
    /\ mask # {}
    /\ mask \subseteq OnlineCpus

CommitLinuxThreadMask ==
    /\ phase = "ready"
    /\ MaskValid(RequestedMask)
    /\ previousMask' = taskMask
    /\ taskMask' = RequestedMask
    /\ migrationPending' = (runningCpu \notin RequestedMask)
    /\ phase' = IF runningCpu \in RequestedMask THEN "ready" ELSE "migrating"
    /\ UNCHANGED <<processMask, runningCpu, childMask, publishedParentMask,
                    childPublished, execGeneration, observedCpu>>

CommitWindowsThreadMask ==
    /\ phase = "ready"
    /\ MaskValid(RequestedMask)
    /\ RequestedMask \subseteq processMask
    /\ previousMask' = taskMask
    /\ taskMask' = RequestedMask
    /\ migrationPending' = (runningCpu \notin RequestedMask)
    /\ phase' = IF runningCpu \in RequestedMask THEN "ready" ELSE "migrating"
    /\ UNCHANGED <<processMask, runningCpu, childMask, publishedParentMask,
                    childPublished, execGeneration, observedCpu>>

CommitWindowsProcessMask ==
    /\ phase = "ready"
    /\ MaskValid(RequestedMask)
    /\ previousMask' = processMask
    /\ processMask' = RequestedMask
    /\ taskMask' = RequestedMask
    /\ migrationPending' = (runningCpu \notin RequestedMask)
    /\ phase' = IF runningCpu \in RequestedMask THEN "ready" ELSE "migrating"
    /\ UNCHANGED <<runningCpu, childMask, publishedParentMask, childPublished,
                    execGeneration, observedCpu>>

RejectInvalidMask ==
    /\ phase = "ready"
    /\ ~MaskValid(InvalidMask)
    /\ previousMask' = {}
    /\ phase' = "rejected"
    /\ UNCHANGED <<processMask, taskMask, runningCpu, migrationPending,
                    childMask, publishedParentMask, childPublished,
                    execGeneration, observedCpu>>

MigrateToAllowedCpu ==
    /\ phase = "migrating"
    /\ migrationPending
    /\ runningCpu' \in taskMask
    /\ observedCpu' = runningCpu'
    /\ migrationPending' = FALSE
    /\ phase' = "ready"
    /\ UNCHANGED <<processMask, taskMask, previousMask, childMask,
                    publishedParentMask, childPublished, execGeneration>>

PublishChild ==
    /\ phase = "ready"
    /\ ~childPublished
    /\ childMask' = taskMask
    /\ publishedParentMask' = taskMask
    /\ childPublished' = TRUE
    /\ UNCHANGED <<processMask, taskMask, previousMask, runningCpu,
                    migrationPending, execGeneration, observedCpu, phase>>

CommitExec ==
    /\ phase = "ready"
    /\ execGeneration < MaxExecGeneration
    /\ execGeneration' = execGeneration + 1
    /\ UNCHANGED <<processMask, taskMask, previousMask, runningCpu,
                    migrationPending, childMask, publishedParentMask,
                    childPublished, observedCpu, phase>>

ObserveCurrentProcessor ==
    /\ phase = "ready"
    /\ observedCpu' = runningCpu
    /\ UNCHANGED <<processMask, taskMask, previousMask, runningCpu,
                    migrationPending, childMask, publishedParentMask,
                    childPublished, execGeneration, phase>>

Terminal ==
    /\ phase = "rejected"
    /\ UNCHANGED vars

Next ==
    \/ CommitLinuxThreadMask
    \/ CommitWindowsThreadMask
    \/ CommitWindowsProcessMask
    \/ RejectInvalidMask
    \/ MigrateToAllowedCpu
    \/ PublishChild
    \/ CommitExec
    \/ ObserveCurrentProcessor
    \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ processMask \subseteq OnlineCpus
    /\ taskMask \subseteq OnlineCpus
    /\ previousMask \subseteq OnlineCpus
    /\ runningCpu \in OnlineCpus
    /\ childMask \subseteq OnlineCpus
    /\ publishedParentMask \subseteq OnlineCpus
    /\ childPublished \in BOOLEAN
    /\ migrationPending \in BOOLEAN
    /\ execGeneration \in 1..MaxExecGeneration
    /\ observedCpu \in OnlineCpus
    /\ phase \in {"ready", "migrating", "rejected"}

LiveMasksAreNonempty ==
    /\ processMask # {}
    /\ taskMask # {}

WindowsThreadMaskStaysInsideProcess ==
    taskMask \subseteq processMask

RunningCpuIsAllowedOrMigrationIsPending ==
    runningCpu \notin taskMask =>
        /\ migrationPending
        /\ phase = "migrating"

NoUserDispatchWhileMigrationPending ==
    phase = "ready" => ~migrationPending

PublishedChildInheritsExactMask ==
    childPublished => childMask = publishedParentMask

CurrentProcessorObservationIsDispatchEligible ==
    phase = "ready" => observedCpu \in taskMask

RejectedRequestPublishesNoPreviousSuccess ==
    phase = "rejected" =>
        /\ previousMask = {}
        /\ ~migrationPending

=============================================================================
