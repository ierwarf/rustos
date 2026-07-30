---------------------- MODULE CpuAffinityObservation ----------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Models topology observation across the Linux and Windows syscall policy
boundaries.

Kernel-compat snapshots the dense Online logical-CPU mask, resolves the exact
target thread's effective mask inside the authenticated caller process, and
stamps both into the syscalld request. Syscalld validates the observation ABI
version, count/mask agreement, target owner, bounds, and reserved fields.
Linux receives the exact effective task mask, which may legitimately contain
one CPU after sched_setaffinity. Windows SystemBasicInformation receives only
the processor count and keeps every Microsoft-reserved output byte zero;
kernelbase derives the active mask from the admitted dense single-group count.
Policy never fabricates CPU zero and raw APIC identifiers never cross either
ABI.

Concrete owners:
  * kernel/hal/src/arch/smp.rs
  * kernel/compat/src/user/syscall/linux/syscalld_ops.rs
  * kernel/compat/src/user/syscall/windows/dispatch.rs
  * services/syscalld/src/affinity_policy.rs
  * libs/rustos-user-abi/src/syscall.rs
  * libs/rustos-user-abi/src/windows.rs
  * compat/windows/services/winsys/kernelbase/exports.c
***************************************************************************)

CONSTANTS LogicalCpus, AdmittedOnline, AdmittedTaskMask, ExpectedVersion,
          ExpectedOwner, ForeignMask, ForeignOwner

VARIABLES requestAbi, stampedMask, stampedCount, stampedVersion,
          stampedTaskMask, stampedOwner, responseMask, responseCount,
          responseReserved, phase
vars == <<requestAbi, stampedMask, stampedCount, stampedVersion,
          stampedTaskMask, stampedOwner, responseMask, responseCount,
          responseReserved, phase>>

Init ==
    /\ requestAbi = "none"
    /\ stampedMask = {}
    /\ stampedCount = 0
    /\ stampedVersion = ExpectedVersion
    /\ stampedTaskMask = {}
    /\ stampedOwner = 0
    /\ responseMask = {}
    /\ responseCount = 0
    /\ responseReserved = 0
    /\ phase = "idle"

StampKernelObservation ==
    /\ phase = "idle"
    /\ requestAbi' \in {"linux", "windows"}
    /\ stampedMask' = AdmittedOnline
    /\ stampedCount' = Cardinality(AdmittedOnline)
    /\ stampedVersion' = ExpectedVersion
    /\ stampedTaskMask' =
        IF requestAbi' = "linux" THEN AdmittedTaskMask ELSE {}
    /\ stampedOwner' =
        IF requestAbi' = "linux" THEN ExpectedOwner ELSE 0
    /\ phase' = "stamped"
    /\ UNCHANGED <<responseMask, responseCount, responseReserved>>

ForgeObservation ==
    /\ phase = "idle"
    /\ requestAbi' \in {"linux", "windows"}
    /\ stampedMask' = ForeignMask
    /\ stampedCount' = Cardinality(AdmittedOnline)
    /\ stampedVersion' = 0
    /\ stampedTaskMask' = ForeignMask
    /\ stampedOwner' = ForeignOwner
    /\ phase' = "stamped"
    /\ UNCHANGED <<responseMask, responseCount, responseReserved>>

ObservationValid ==
    /\ stampedVersion = ExpectedVersion
    /\ stampedMask = AdmittedOnline
    /\ stampedMask \subseteq LogicalCpus
    /\ stampedCount = Cardinality(stampedMask)
    /\ stampedCount \in 1..8
    /\ IF requestAbi = "linux" THEN
          /\ stampedTaskMask # {}
          /\ stampedTaskMask \subseteq stampedMask
          /\ stampedOwner = ExpectedOwner
       ELSE
          /\ stampedTaskMask = {}
          /\ stampedOwner = 0

PublishAffinity ==
    /\ phase = "stamped"
    /\ ObservationValid
    /\ responseMask' = IF requestAbi = "linux" THEN stampedTaskMask ELSE {}
    /\ responseCount' = IF requestAbi = "windows" THEN stampedCount ELSE 0
    /\ responseReserved' = 0
    /\ phase' = "replied"
    /\ UNCHANGED <<requestAbi, stampedMask, stampedCount, stampedVersion,
                    stampedTaskMask, stampedOwner>>

RejectObservation ==
    /\ phase = "stamped"
    /\ ~ObservationValid
    /\ phase' = "rejected"
    /\ UNCHANGED <<requestAbi, stampedMask, stampedCount, stampedVersion,
                    stampedTaskMask, stampedOwner, responseMask,
                    responseCount, responseReserved>>

Terminal ==
    /\ phase \in {"replied", "rejected"}
    /\ UNCHANGED vars

Next ==
    \/ StampKernelObservation
    \/ ForgeObservation
    \/ PublishAffinity
    \/ RejectObservation
    \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ requestAbi \in {"none", "linux", "windows"}
    /\ stampedMask \subseteq LogicalCpus
    /\ stampedCount \in 0..8
    /\ stampedVersion \in {0, ExpectedVersion}
    /\ stampedTaskMask \subseteq LogicalCpus
    /\ stampedOwner \in {0, ExpectedOwner, ForeignOwner}
    /\ responseMask \subseteq LogicalCpus
    /\ responseCount \in 0..8
    /\ responseReserved \in Nat
    /\ phase \in {"idle", "stamped", "replied", "rejected"}

LinuxReplyIsExactEffectiveTaskMask ==
    phase = "replied" /\ requestAbi = "linux" =>
        responseMask = AdmittedTaskMask

LinuxReplyStaysInsideOnlineSet ==
    phase = "replied" /\ requestAbi = "linux" =>
        /\ responseMask # {}
        /\ responseMask \subseteq AdmittedOnline

RejectedObservationPublishesNothing ==
    phase = "rejected" =>
        /\ responseMask = {}
        /\ responseCount = 0
        /\ responseReserved = 0

WindowsPublishesOnlyExactCount ==
    phase = "replied" /\ requestAbi = "windows" =>
        /\ responseMask = {}
        /\ responseCount = Cardinality(AdmittedOnline)
        /\ responseReserved = 0

=============================================================================
