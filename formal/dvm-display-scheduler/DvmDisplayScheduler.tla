---------------------- MODULE DvmDisplayScheduler ----------------------
EXTENDS Naturals

(*******************************************************************************
Bounded scheduler admission for the authenticated Linux display-DVM relay.

The display process starts under the normal policy. Only confirmation of the
host invitation may admit its current relay thread to SCHED_RR. The display
priority stays below the authenticated input relay, and RLIMIT_RTTIME bounds a
thread that stops reaching blocking DRM/fence operations. Stop, hard-limit, or
relay failure withdraws realtime policy before the process retries.
*******************************************************************************)

CONSTANTS NormalPolicy, RoundRobinPolicy, DisplayPriority, InputPriority,
          SoftBudgetUs, HardBudgetUs, MaxCpuTicks

VARIABLES authenticated,
          policy,
          priority,
          budgetBounded,
          relayActive,
          continuousCpuTicks,
          stopPending,
          restored

vars == <<authenticated, policy, priority, budgetBounded, relayActive,
          continuousCpuTicks, stopPending, restored>>

Init ==
    /\ authenticated = FALSE
    /\ policy = NormalPolicy
    /\ priority = 0
    /\ budgetBounded = FALSE
    /\ relayActive = FALSE
    /\ continuousCpuTicks = 0
    /\ stopPending = FALSE
    /\ restored = FALSE

AuthenticateHost ==
    /\ ~authenticated
    /\ authenticated' = TRUE
    /\ UNCHANGED <<policy, priority, budgetBounded, relayActive,
                  continuousCpuTicks, stopPending, restored>>

AdmitScheduler ==
    /\ authenticated
    /\ policy = NormalPolicy
    /\ ~relayActive
    /\ ~stopPending
    /\ policy' = RoundRobinPolicy
    /\ priority' = DisplayPriority
    /\ budgetBounded' = TRUE
    /\ continuousCpuTicks' = 0
    /\ restored' = FALSE
    /\ UNCHANGED <<authenticated, relayActive, stopPending>>

StartRelay ==
    /\ authenticated
    /\ policy = RoundRobinPolicy
    /\ budgetBounded
    /\ ~relayActive
    /\ ~stopPending
    /\ relayActive' = TRUE
    /\ UNCHANGED <<authenticated, policy, priority, budgetBounded,
                  continuousCpuTicks, stopPending, restored>>

ConsumeCpu ==
    /\ relayActive
    /\ continuousCpuTicks < MaxCpuTicks
    /\ continuousCpuTicks' = continuousCpuTicks + 1
    /\ UNCHANGED <<authenticated, policy, priority, budgetBounded,
                  relayActive, stopPending, restored>>

BlockingFenceOrDrm ==
    /\ relayActive
    /\ continuousCpuTicks # 0
    /\ continuousCpuTicks' = 0
    /\ UNCHANGED <<authenticated, policy, priority, budgetBounded,
                  relayActive, stopPending, restored>>

RequestStop ==
    /\ relayActive
    /\ relayActive' = FALSE
    /\ stopPending' = TRUE
    /\ UNCHANGED <<authenticated, policy, priority, budgetBounded,
                  continuousCpuTicks, restored>>

HardLimitStop ==
    /\ relayActive
    /\ continuousCpuTicks = MaxCpuTicks
    /\ relayActive' = FALSE
    /\ stopPending' = TRUE
    /\ UNCHANGED <<authenticated, policy, priority, budgetBounded,
                  continuousCpuTicks, restored>>

RestoreScheduler ==
    /\ stopPending
    /\ policy = RoundRobinPolicy
    /\ policy' = NormalPolicy
    /\ priority' = 0
    /\ budgetBounded' = FALSE
    /\ continuousCpuTicks' = 0
    /\ stopPending' = FALSE
    /\ restored' = TRUE
    /\ UNCHANGED <<authenticated, relayActive>>

Next ==
    \/ AuthenticateHost
    \/ AdmitScheduler
    \/ StartRelay
    \/ ConsumeCpu
    \/ BlockingFenceOrDrm
    \/ RequestStop
    \/ HardLimitStop
    \/ RestoreScheduler

Spec == Init /\ [][Next]_vars
        /\ WF_vars(HardLimitStop)
        /\ WF_vars(RestoreScheduler)

TypeOK ==
    /\ authenticated \in BOOLEAN
    /\ policy \in {NormalPolicy, RoundRobinPolicy}
    /\ priority \in {0, DisplayPriority}
    /\ budgetBounded \in BOOLEAN
    /\ relayActive \in BOOLEAN
    /\ continuousCpuTicks \in 0..MaxCpuTicks
    /\ stopPending \in BOOLEAN
    /\ restored \in BOOLEAN

RealtimeRequiresAuthentication ==
    policy = RoundRobinPolicy => authenticated

ActiveRelayRequiresBoundedRealtime ==
    relayActive =>
        /\ authenticated
        /\ policy = RoundRobinPolicy
        /\ priority = DisplayPriority
        /\ budgetBounded
        /\ ~stopPending

DisplayNeverOutranksInput ==
    policy = RoundRobinPolicy => DisplayPriority < InputPriority

BudgetContractExact ==
    /\ SoftBudgetUs = 50000
    /\ HardBudgetUs = 100000
    /\ SoftBudgetUs < HardBudgetUs

StoppedRelayDoesNotRun == stopPending => ~relayActive

EveryStopRestoresNormalPolicy ==
    [](stopPending => <> (restored /\ policy = NormalPolicy))

=============================================================================
