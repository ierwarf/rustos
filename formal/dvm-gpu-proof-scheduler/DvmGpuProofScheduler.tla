---------------------- MODULE DvmGpuProofScheduler ----------------------
EXTENDS Naturals

(*******************************************************************************
Models bounded scheduler admission for the private AMD/virtio GPU proof.

Concrete owner:
  * driver-domains/linux/package/rustos-dvm-display/src/
    rustos-dvm-gpu-probe.c

Pipeline creation and its acquire fence complete under the normal policy. Only
the finite post-prime performance proof may enter SCHED_RR, below both the
authenticated display and input relays and with an exact RLIMIT_RTTIME bound.
Success and ordinary failure pass through one restore state. The Linux hard
CPU limit instead terminates the proof process, removing all scheduler
authority and leaving no readiness evidence for the init owner to accept.
Performance evidence can be published only after normal policy is observed
again. The long-lived health loop never retains realtime authority.
RLIMIT_RTTIME installation is modeled separately from SCHED_RR admission.
Admission rollback and normal-policy restoration are accepted only after exact
readback; an uncertain restore terminates the process without evidence.
*******************************************************************************)

CONSTANTS NormalPolicy, RoundRobinPolicy, NoPolicy, ProofPriority, DisplayPriority,
          InputPriority, SoftBudgetUs, HardBudgetUs, MaxCpuTicks

Boot == "boot"
Primed == "primed"
Limiting == "limiting"
Measuring == "measuring"
Restoring == "restoring"
Measured == "measured"
Published == "published"
Failed == "failed"
Terminated == "terminated"

NoResult == "none"
Pass == "pass"
Fail == "fail"

VARIABLES phase,
          policy,
          priority,
          budgetBounded,
          continuousCpuTicks,
          result,
          hardLimitReached,
          restoreFailed,
          evidencePublished,
          healthEpoch

vars == <<phase, policy, priority, budgetBounded, continuousCpuTicks,
          result, hardLimitReached, restoreFailed, evidencePublished,
          healthEpoch>>

Init ==
    /\ phase = Boot
    /\ policy = NormalPolicy
    /\ priority = 0
    /\ budgetBounded = FALSE
    /\ continuousCpuTicks = 0
    /\ result = NoResult
    /\ hardLimitReached = FALSE
    /\ restoreFailed = FALSE
    /\ evidencePublished = FALSE
    /\ healthEpoch = 0

CompletePrime ==
    /\ phase = Boot
    /\ policy = NormalPolicy
    /\ phase' = Primed
    /\ UNCHANGED <<policy, priority, budgetBounded, continuousCpuTicks,
                  result, hardLimitReached, restoreFailed, evidencePublished,
                  healthEpoch>>

InstallProofBudget ==
    /\ phase = Primed
    /\ policy = NormalPolicy
    /\ priority = 0
    /\ ~budgetBounded
    /\ phase' = Limiting
    /\ budgetBounded' = TRUE
    /\ UNCHANGED <<policy, priority, continuousCpuTicks, result,
                  hardLimitReached, restoreFailed, evidencePublished,
                  healthEpoch>>

AdmitProofScheduler ==
    /\ phase = Limiting
    /\ policy = NormalPolicy
    /\ priority = 0
    /\ budgetBounded
    /\ phase' = Measuring
    /\ policy' = RoundRobinPolicy
    /\ priority' = ProofPriority
    /\ continuousCpuTicks' = 0
    /\ UNCHANGED <<budgetBounded, result, hardLimitReached, restoreFailed,
                  evidencePublished, healthEpoch>>

RollbackAdmission ==
    /\ phase = Limiting
    /\ policy = NormalPolicy
    /\ budgetBounded
    /\ phase' = Failed
    /\ budgetBounded' = FALSE
    /\ result' = Fail
    /\ UNCHANGED <<policy, priority, continuousCpuTicks, hardLimitReached,
                  restoreFailed, evidencePublished, healthEpoch>>

FatalAdmissionRollback ==
    /\ phase = Limiting
    /\ policy = NormalPolicy
    /\ budgetBounded
    /\ phase' = Terminated
    /\ policy' = NoPolicy
    /\ priority' = 0
    /\ budgetBounded' = FALSE
    /\ continuousCpuTicks' = 0
    /\ result' = Fail
    /\ restoreFailed' = TRUE
    /\ UNCHANGED <<hardLimitReached, evidencePublished, healthEpoch>>

ConsumeCpu ==
    /\ phase = Measuring
    /\ continuousCpuTicks < MaxCpuTicks
    /\ continuousCpuTicks' = continuousCpuTicks + 1
    /\ UNCHANGED <<phase, policy, priority, budgetBounded, result,
                  hardLimitReached, restoreFailed, evidencePublished,
                  healthEpoch>>

BlockingFenceWait ==
    /\ phase = Measuring
    /\ continuousCpuTicks # 0
    /\ continuousCpuTicks' = 0
    /\ UNCHANGED <<phase, policy, priority, budgetBounded, result,
                  hardLimitReached, restoreFailed, evidencePublished,
                  healthEpoch>>

RejectSchedulerObservation ==
    /\ phase = Measuring
    /\ continuousCpuTicks = 0
    /\ result = NoResult
    /\ phase' = Restoring
    /\ result' = Fail
    /\ UNCHANGED <<policy, priority, budgetBounded, continuousCpuTicks,
                  hardLimitReached, restoreFailed, evidencePublished,
                  healthEpoch>>

CompleteMeasurement ==
    /\ phase = Measuring
    /\ continuousCpuTicks < MaxCpuTicks
    /\ phase' = Restoring
    /\ result' = Pass
    /\ UNCHANGED <<policy, priority, budgetBounded, continuousCpuTicks,
                  hardLimitReached, restoreFailed, evidencePublished,
                  healthEpoch>>

FailMeasurement ==
    /\ phase = Measuring
    /\ phase' = Restoring
    /\ result' = Fail
    /\ UNCHANGED <<policy, priority, budgetBounded, continuousCpuTicks,
                  hardLimitReached, restoreFailed, evidencePublished,
                  healthEpoch>>

HardLimitStop ==
    /\ phase = Measuring
    /\ continuousCpuTicks = MaxCpuTicks
    /\ phase' = Terminated
    /\ result' = Fail
    /\ hardLimitReached' = TRUE
    /\ policy' = NoPolicy
    /\ priority' = 0
    /\ budgetBounded' = FALSE
    /\ continuousCpuTicks' = 0
    /\ restoreFailed' = FALSE
    /\ UNCHANGED <<evidencePublished, healthEpoch>>

RestoreNormalPolicy ==
    /\ phase = Restoring
    /\ policy = RoundRobinPolicy
    /\ phase' = IF result = Pass THEN Measured ELSE Failed
    /\ policy' = NormalPolicy
    /\ priority' = 0
    /\ budgetBounded' = FALSE
    /\ continuousCpuTicks' = 0
    /\ restoreFailed' = FALSE
    /\ UNCHANGED <<result, hardLimitReached, evidencePublished, healthEpoch>>

FatalRestoreFailure ==
    /\ phase = Restoring
    /\ policy = RoundRobinPolicy
    /\ phase' = Terminated
    /\ policy' = NoPolicy
    /\ priority' = 0
    /\ budgetBounded' = FALSE
    /\ continuousCpuTicks' = 0
    /\ result' = Fail
    /\ restoreFailed' = TRUE
    /\ UNCHANGED <<hardLimitReached, evidencePublished, healthEpoch>>

PublishEvidence ==
    /\ phase = Measured
    /\ result = Pass
    /\ policy = NormalPolicy
    /\ ~budgetBounded
    /\ phase' = Published
    /\ evidencePublished' = TRUE
    /\ UNCHANGED <<policy, priority, budgetBounded, continuousCpuTicks,
                  result, hardLimitReached, restoreFailed, healthEpoch>>

HealthSubmission ==
    /\ phase = Published
    /\ policy = NormalPolicy
    /\ healthEpoch' = (healthEpoch + 1) % 3
    /\ UNCHANGED <<phase, policy, priority, budgetBounded,
                  continuousCpuTicks, result, hardLimitReached,
                  restoreFailed, evidencePublished>>

AdmissionSettlement ==
    AdmitProofScheduler \/ RollbackAdmission \/ FatalAdmissionRollback
RestoreSettlement == RestoreNormalPolicy \/ FatalRestoreFailure

Next ==
    \/ CompletePrime
    \/ InstallProofBudget
    \/ AdmitProofScheduler
    \/ RollbackAdmission
    \/ FatalAdmissionRollback
    \/ ConsumeCpu
    \/ BlockingFenceWait
    \/ RejectSchedulerObservation
    \/ CompleteMeasurement
    \/ FailMeasurement
    \/ HardLimitStop
    \/ RestoreNormalPolicy
    \/ FatalRestoreFailure
    \/ PublishEvidence
    \/ HealthSubmission

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(HardLimitStop)
    /\ WF_vars(AdmissionSettlement)
    /\ WF_vars(RestoreSettlement)
    /\ WF_vars(PublishEvidence)

TypeOK ==
    /\ phase \in {Boot, Primed, Limiting, Measuring, Restoring, Measured,
                   Published, Failed, Terminated}
    /\ policy \in {NormalPolicy, RoundRobinPolicy, NoPolicy}
    /\ priority \in {0, ProofPriority}
    /\ budgetBounded \in BOOLEAN
    /\ continuousCpuTicks \in 0..MaxCpuTicks
    /\ result \in {NoResult, Pass, Fail}
    /\ hardLimitReached \in BOOLEAN
    /\ restoreFailed \in BOOLEAN
    /\ evidencePublished \in BOOLEAN
    /\ healthEpoch \in 0..2

RealtimeIsMeasurementOnly ==
    (policy = RoundRobinPolicy) =>
        /\ phase \in {Measuring, Restoring}
        /\ priority = ProofPriority
        /\ budgetBounded
        /\ ~restoreFailed

LimitingHasNoRealtimeAuthority ==
    phase = Limiting =>
        /\ policy = NormalPolicy
        /\ priority = 0
        /\ budgetBounded
        /\ result = NoResult
        /\ ~restoreFailed

ProofNeverOutranksRelays ==
    /\ ProofPriority < DisplayPriority
    /\ DisplayPriority < InputPriority

BudgetContractExact ==
    /\ SoftBudgetUs = 50000
    /\ HardBudgetUs = 100000
    /\ SoftBudgetUs < HardBudgetUs

EvidenceRequiresRestoredSuccess ==
    evidencePublished =>
        /\ phase = Published
        /\ result = Pass
        /\ policy = NormalPolicy
        /\ priority = 0
        /\ ~budgetBounded
        /\ ~hardLimitReached
        /\ ~restoreFailed

HealthNeverRetainsRealtime ==
    phase = Published => policy = NormalPolicy

HardLimitTerminatesWithoutEvidence ==
    hardLimitReached =>
        /\ phase = Terminated
        /\ policy = NoPolicy
        /\ priority = 0
        /\ ~budgetBounded
        /\ ~evidencePublished

RestoreFailureTerminatesWithoutEvidence ==
    restoreFailed =>
        /\ phase = Terminated
        /\ policy = NoPolicy
        /\ priority = 0
        /\ ~budgetBounded
        /\ ~evidencePublished

EveryOrdinaryResultSettles ==
    [](phase = Restoring => <>
        ((policy = NormalPolicy /\ phase # Restoring /\ ~budgetBounded) \/
         (restoreFailed /\ phase = Terminated)))

SuccessfulMeasurementIsEventuallyPublished ==
    [](phase = Measured => <> evidencePublished)

=============================================================================
