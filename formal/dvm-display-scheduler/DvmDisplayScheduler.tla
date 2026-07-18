---------------------- MODULE DvmDisplayScheduler ----------------------
EXTENDS Naturals

(*******************************************************************************
Bounded scheduler admission for the authenticated Linux display-DVM relay.

The display process starts under the normal policy. Only confirmation of the
host invitation may admit its current relay thread to SCHED_RR. The display
priority stays below the authenticated input relay, and RLIMIT_RTTIME bounds a
thread that stops reaching blocking DRM/fence operations. Stop or ordinary
relay failure restores normal policy before retry. The Linux hard limit kills
the process and therefore removes all scheduler and relay authority. Admission
is deliberately two-phase because Linux installs RLIMIT_RTTIME before changing
the scheduler. A failed rollback or a restore/readback mismatch is fatal; the
same process may not retry with uncertain realtime authority.
*******************************************************************************)

CONSTANTS NormalPolicy, RoundRobinPolicy, NoPolicy, DisplayPriority, InputPriority,
          SoftBudgetUs, HardBudgetUs, MaxCpuTicks

VARIABLES authenticated,
          policy,
          priority,
          budgetBounded,
          relayActive,
          continuousCpuTicks,
          admissionPending,
          stopPending,
          restored,
          restoreFailed,
          terminated

vars == <<authenticated, policy, priority, budgetBounded, relayActive,
          continuousCpuTicks, admissionPending, stopPending, restored,
          restoreFailed, terminated>>

Init ==
    /\ authenticated = FALSE
    /\ policy = NormalPolicy
    /\ priority = 0
    /\ budgetBounded = FALSE
    /\ relayActive = FALSE
    /\ continuousCpuTicks = 0
    /\ admissionPending = FALSE
    /\ stopPending = FALSE
    /\ restored = FALSE
    /\ restoreFailed = FALSE
    /\ terminated = FALSE

AuthenticateHost ==
    /\ ~authenticated
    /\ ~terminated
    /\ authenticated' = TRUE
    /\ UNCHANGED <<policy, priority, budgetBounded, relayActive,
                  continuousCpuTicks, admissionPending, stopPending, restored,
                  restoreFailed, terminated>>

InstallBudget ==
    /\ authenticated
    /\ ~terminated
    /\ policy = NormalPolicy
    /\ ~budgetBounded
    /\ ~relayActive
    /\ ~admissionPending
    /\ ~stopPending
    /\ budgetBounded' = TRUE
    /\ admissionPending' = TRUE
    /\ restored' = FALSE
    /\ UNCHANGED <<authenticated, policy, priority, relayActive,
                  continuousCpuTicks, stopPending, restoreFailed, terminated>>

AdmitScheduler ==
    /\ authenticated
    /\ ~terminated
    /\ policy = NormalPolicy
    /\ budgetBounded
    /\ admissionPending
    /\ ~relayActive
    /\ ~stopPending
    /\ policy' = RoundRobinPolicy
    /\ priority' = DisplayPriority
    /\ admissionPending' = FALSE
    /\ continuousCpuTicks' = 0
    /\ restored' = FALSE
    /\ UNCHANGED <<authenticated, budgetBounded, relayActive, stopPending,
                  restoreFailed, terminated>>

RollbackAdmission ==
    /\ authenticated
    /\ ~terminated
    /\ policy = NormalPolicy
    /\ budgetBounded
    /\ admissionPending
    /\ budgetBounded' = FALSE
    /\ admissionPending' = FALSE
    /\ restored' = TRUE
    /\ UNCHANGED <<authenticated, policy, priority, relayActive,
                  continuousCpuTicks, stopPending, restoreFailed, terminated>>

FatalAdmissionRollback ==
    /\ ~terminated
    /\ policy = NormalPolicy
    /\ budgetBounded
    /\ admissionPending
    /\ authenticated' = FALSE
    /\ policy' = NoPolicy
    /\ priority' = 0
    /\ budgetBounded' = FALSE
    /\ relayActive' = FALSE
    /\ continuousCpuTicks' = 0
    /\ admissionPending' = FALSE
    /\ stopPending' = FALSE
    /\ restored' = FALSE
    /\ restoreFailed' = TRUE
    /\ terminated' = TRUE

StartRelay ==
    /\ authenticated
    /\ ~terminated
    /\ policy = RoundRobinPolicy
    /\ budgetBounded
    /\ ~admissionPending
    /\ ~relayActive
    /\ ~stopPending
    /\ relayActive' = TRUE
    /\ UNCHANGED <<authenticated, policy, priority, budgetBounded,
                  continuousCpuTicks, admissionPending, stopPending, restored,
                  restoreFailed, terminated>>

ConsumeCpu ==
    /\ relayActive
    /\ continuousCpuTicks < MaxCpuTicks
    /\ continuousCpuTicks' = continuousCpuTicks + 1
    /\ UNCHANGED <<authenticated, policy, priority, budgetBounded,
                  relayActive, admissionPending, stopPending, restored,
                  restoreFailed, terminated>>

BlockingFenceOrDrm ==
    /\ relayActive
    /\ continuousCpuTicks # 0
    /\ continuousCpuTicks' = 0
    /\ UNCHANGED <<authenticated, policy, priority, budgetBounded,
                  relayActive, admissionPending, stopPending, restored,
                  restoreFailed, terminated>>

RejectSchedulerObservation ==
    /\ authenticated
    /\ ~terminated
    /\ policy = RoundRobinPolicy
    /\ budgetBounded
    /\ ~relayActive
    /\ ~admissionPending
    /\ ~stopPending
    /\ stopPending' = TRUE
    /\ restored' = FALSE
    /\ UNCHANGED <<authenticated, policy, priority, budgetBounded, relayActive,
                  continuousCpuTicks, admissionPending, restoreFailed,
                  terminated>>

RequestStop ==
    /\ relayActive
    /\ relayActive' = FALSE
    /\ stopPending' = TRUE
    /\ UNCHANGED <<authenticated, policy, priority, budgetBounded,
                  continuousCpuTicks, admissionPending, restored,
                  restoreFailed, terminated>>

HardLimitStop ==
    /\ relayActive
    /\ continuousCpuTicks = MaxCpuTicks
    /\ relayActive' = FALSE
    /\ authenticated' = FALSE
    /\ policy' = NoPolicy
    /\ priority' = 0
    /\ budgetBounded' = FALSE
    /\ continuousCpuTicks' = 0
    /\ admissionPending' = FALSE
    /\ stopPending' = FALSE
    /\ restored' = FALSE
    /\ restoreFailed' = FALSE
    /\ terminated' = TRUE

RestoreScheduler ==
    /\ stopPending
    /\ ~terminated
    /\ policy = RoundRobinPolicy
    /\ policy' = NormalPolicy
    /\ priority' = 0
    /\ budgetBounded' = FALSE
    /\ continuousCpuTicks' = 0
    /\ stopPending' = FALSE
    /\ restored' = TRUE
    /\ restoreFailed' = FALSE
    /\ UNCHANGED <<authenticated, relayActive, admissionPending, terminated>>

FatalRestoreFailure ==
    /\ stopPending
    /\ ~terminated
    /\ policy = RoundRobinPolicy
    /\ authenticated' = FALSE
    /\ policy' = NoPolicy
    /\ priority' = 0
    /\ budgetBounded' = FALSE
    /\ relayActive' = FALSE
    /\ continuousCpuTicks' = 0
    /\ admissionPending' = FALSE
    /\ stopPending' = FALSE
    /\ restored' = FALSE
    /\ restoreFailed' = TRUE
    /\ terminated' = TRUE

AdmissionSettlement == RollbackAdmission \/ FatalAdmissionRollback
RestoreSettlement == RestoreScheduler \/ FatalRestoreFailure

Next ==
    \/ AuthenticateHost
    \/ InstallBudget
    \/ AdmitScheduler
    \/ RollbackAdmission
    \/ FatalAdmissionRollback
    \/ StartRelay
    \/ ConsumeCpu
    \/ BlockingFenceOrDrm
    \/ RejectSchedulerObservation
    \/ RequestStop
    \/ HardLimitStop
    \/ RestoreScheduler
    \/ FatalRestoreFailure

Spec == Init /\ [][Next]_vars
        /\ WF_vars(HardLimitStop)
        /\ WF_vars(AdmissionSettlement)
        /\ WF_vars(RestoreSettlement)

TypeOK ==
    /\ authenticated \in BOOLEAN
    /\ policy \in {NormalPolicy, RoundRobinPolicy, NoPolicy}
    /\ priority \in {0, DisplayPriority}
    /\ budgetBounded \in BOOLEAN
    /\ relayActive \in BOOLEAN
    /\ continuousCpuTicks \in 0..MaxCpuTicks
    /\ admissionPending \in BOOLEAN
    /\ stopPending \in BOOLEAN
    /\ restored \in BOOLEAN
    /\ restoreFailed \in BOOLEAN
    /\ terminated \in BOOLEAN

RealtimeRequiresVerifiedBound ==
    policy = RoundRobinPolicy =>
        /\ authenticated
        /\ budgetBounded
        /\ ~admissionPending
        /\ ~restoreFailed

PartialAdmissionHasNoRelay ==
    admissionPending =>
        /\ authenticated
        /\ policy = NormalPolicy
        /\ priority = 0
        /\ budgetBounded
        /\ ~relayActive
        /\ ~stopPending
        /\ ~restoreFailed
        /\ ~terminated

ActiveRelayRequiresBoundedRealtime ==
    relayActive =>
        /\ authenticated
        /\ policy = RoundRobinPolicy
        /\ priority = DisplayPriority
        /\ budgetBounded
        /\ ~admissionPending
        /\ ~stopPending
        /\ ~restoreFailed
        /\ ~terminated

DisplayNeverOutranksInput ==
    policy = RoundRobinPolicy => DisplayPriority < InputPriority

BudgetContractExact ==
    /\ SoftBudgetUs = 50000
    /\ HardBudgetUs = 100000
    /\ SoftBudgetUs < HardBudgetUs

StoppedRelayDoesNotRun == stopPending => ~relayActive

RetryableNormalStateHasNoResidualBound ==
    policy = NormalPolicy /\ ~admissionPending /\ ~stopPending => ~budgetBounded

RestoreFailureTerminatesWithoutAuthority ==
    restoreFailed =>
        /\ terminated
        /\ policy = NoPolicy
        /\ priority = 0
        /\ ~budgetBounded
        /\ ~relayActive
        /\ ~authenticated

TerminatedProcessHasNoAuthority ==
    terminated =>
        /\ ~authenticated
        /\ ~relayActive
        /\ ~admissionPending
        /\ ~stopPending
        /\ policy = NoPolicy
        /\ priority = 0
        /\ ~budgetBounded

EveryStopSettlesBeforeRetry ==
    [](stopPending => <>
        ((restored /\ policy = NormalPolicy /\ ~budgetBounded) \/
         (restoreFailed /\ terminated)))

=============================================================================
