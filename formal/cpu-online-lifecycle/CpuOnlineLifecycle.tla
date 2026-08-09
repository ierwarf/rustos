---------------------- MODULE CpuOnlineLifecycle ----------------------
EXTENDS Naturals

(***************************************************************************
Models the boot-static generation-bound CPU online state machine. No CPU may
publish scheduling authority before its private architectural state and
scheduler substrate are ready. IA32_PAT is per logical CPU, so the BSP and
every AP program and exactly read back slot 0 as WB, slot 2 as UC, and slot 4
as WC before private-ready publication or dispatch. The shared-RAM WB mapping
uses slot 0 and does not depend on the WC selector in slot 4. INIT leaves an AP
cache-disabled; the AP must first enter CD=1/NW=0 no-fill mode, restore the
sealed BSP MTRR/PAT baseline, and only then enable caching. No AP can publish
private readiness from reset cache state or a reset/foreign MTRR baseline.

Concrete owner:
  * kernel/hal/src/arch/smp.rs
  * kernel/mm/src/memory/cache_attributes.rs
  * kernel/mm/src/memory/kernel_vm.rs
***************************************************************************)

CONSTANTS Cpus, Bsp

Discovered == "discovered"
Starting == "starting"
OnlineParked == "online-parked"
SchedulerReady == "scheduler-ready"
Online == "online"
Quarantined == "quarantined"
Failed == "failed"

PatValues == {"unset", "wb", "non-wb", "uc", "wc"}
PatStates == [slot0 : PatValues, slot2 : PatValues, slot4 : PatValues]
PatUnread == [slot0 |-> "unset", slot2 |-> "unset", slot4 |-> "unset"]
PatKernelCacheContract == [slot0 |-> "wb", slot2 |-> "uc", slot4 |-> "wc"]

WbSelector == "slot0-wb"
WcSlot4Selector == "slot4-wc"
UnmappedSelector == "unmapped"
CacheSelectors == {UnmappedSelector, WbSelector, WcSlot4Selector}
PatRequiredStates == {OnlineParked, SchedulerReady, Online, Quarantined}

ResetCacheDisabled == "reset-cache-disabled"
NoFillCache == "no-fill-cache"
CacheEnabled == "cache-enabled"
CacheStates == {ResetCacheDisabled, NoFillCache, CacheEnabled}

VARIABLES state, generation, privateReady, schedulerReady, dispatchAuthority,
          failedSeen, staleGenerationAttempted, restartAttempted, panicRaised,
          initialPatSlot0, patSlot0Validated, patProgrammed, patReadback,
          sharedMemoryWbMapped,
          sharedMemorySelector, bspMemoryTypeBaselineCaptured,
          mtrrBaselineRestored, cacheState

vars == <<state, generation, privateReady, schedulerReady, dispatchAuthority,
          failedSeen, staleGenerationAttempted, restartAttempted, panicRaised,
          initialPatSlot0, patSlot0Validated, patProgrammed, patReadback,
          sharedMemoryWbMapped,
          sharedMemorySelector, bspMemoryTypeBaselineCaptured,
          mtrrBaselineRestored, cacheState>>

PatKernelCacheContractReady(cpu) ==
    patSlot0Validated[cpu]
    /\ patProgrammed[cpu] = PatKernelCacheContract
    /\ patReadback[cpu] = PatKernelCacheContract

CpuMemoryTypeContractReady(cpu) ==
    /\ bspMemoryTypeBaselineCaptured
    /\ mtrrBaselineRestored[cpu]
    /\ cacheState[cpu] = CacheEnabled
    /\ PatKernelCacheContractReady(cpu)

Init ==
    /\ state = [cpu \in Cpus |-> Discovered]
    /\ generation = [cpu \in Cpus |-> 1]
    /\ privateReady = [cpu \in Cpus |-> FALSE]
    /\ schedulerReady = [cpu \in Cpus |-> FALSE]
    /\ dispatchAuthority = [cpu \in Cpus |-> FALSE]
    /\ failedSeen = [cpu \in Cpus |-> FALSE]
    /\ staleGenerationAttempted = FALSE
    /\ restartAttempted = FALSE
    /\ panicRaised = FALSE
    /\ initialPatSlot0 \in [Cpus -> {"wb", "non-wb"}]
    /\ patSlot0Validated = [cpu \in Cpus |-> FALSE]
    /\ patProgrammed = [cpu \in Cpus |->
          [slot0 |-> initialPatSlot0[cpu],
           slot2 |-> "unset", slot4 |-> "unset"]]
    /\ patReadback = [cpu \in Cpus |-> PatUnread]
    /\ sharedMemoryWbMapped = FALSE
    /\ sharedMemorySelector = UnmappedSelector
    /\ bspMemoryTypeBaselineCaptured = FALSE
    /\ mtrrBaselineRestored = [cpu \in Cpus |-> FALSE]
    /\ cacheState = [cpu \in Cpus |->
          IF cpu = Bsp THEN CacheEnabled ELSE ResetCacheDisabled]

Start(cpu) ==
    /\ state[cpu] = Discovered
    /\ state' = [state EXCEPT ![cpu] = Starting]
    /\ UNCHANGED <<generation, privateReady, schedulerReady, dispatchAuthority,
                   failedSeen, staleGenerationAttempted, restartAttempted,
                   panicRaised, initialPatSlot0, patSlot0Validated,
                   patProgrammed, patReadback,
                   sharedMemoryWbMapped, sharedMemorySelector,
                   bspMemoryTypeBaselineCaptured, mtrrBaselineRestored,
                   cacheState>>

EnterApNoFillCache(cpu) ==
    /\ cpu # Bsp
    /\ state[cpu] = Starting
    /\ cacheState[cpu] = ResetCacheDisabled
    /\ cacheState' = [cacheState EXCEPT ![cpu] = NoFillCache]
    /\ UNCHANGED <<state, generation, privateReady, schedulerReady,
                   dispatchAuthority, failedSeen, staleGenerationAttempted,
                   restartAttempted, panicRaised, initialPatSlot0,
                   patSlot0Validated, patProgrammed, patReadback,
                   sharedMemoryWbMapped, sharedMemorySelector,
                   bspMemoryTypeBaselineCaptured, mtrrBaselineRestored>>

ValidatePatSlot0Wb(cpu) ==
    /\ state[cpu] = Starting
    /\ (cpu = Bsp \/ cacheState[cpu] = NoFillCache)
    /\ initialPatSlot0[cpu] = "wb"
    /\ ~patSlot0Validated[cpu]
    /\ patSlot0Validated' = [patSlot0Validated EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<state, generation, privateReady, schedulerReady,
                   dispatchAuthority, failedSeen, staleGenerationAttempted,
                   restartAttempted, panicRaised, initialPatSlot0,
                   patProgrammed, patReadback, sharedMemoryWbMapped,
                   sharedMemorySelector, bspMemoryTypeBaselineCaptured,
                   mtrrBaselineRestored, cacheState>>

RejectInvalidPatSlot0(cpu) ==
    /\ state[cpu] = Starting
    /\ (cpu = Bsp \/ cacheState[cpu] = NoFillCache)
    /\ initialPatSlot0[cpu] # "wb"
    /\ panicRaised' = TRUE
    /\ UNCHANGED <<state, generation, privateReady, schedulerReady,
                   dispatchAuthority, failedSeen, staleGenerationAttempted,
                   restartAttempted, initialPatSlot0, patSlot0Validated,
                   patProgrammed, patReadback, sharedMemoryWbMapped,
                   sharedMemorySelector, bspMemoryTypeBaselineCaptured,
                   mtrrBaselineRestored, cacheState>>

ProgramPatKernelCacheContract(cpu) ==
    /\ state[cpu] = Starting
    /\ (cpu = Bsp \/ cacheState[cpu] = NoFillCache)
    /\ patSlot0Validated[cpu]
    /\ patProgrammed[cpu].slot2 = "unset"
    /\ patProgrammed[cpu].slot4 = "unset"
    /\ patProgrammed' =
          [patProgrammed EXCEPT ![cpu] =
              [slot0 |-> patProgrammed[cpu].slot0,
               slot2 |-> "uc", slot4 |-> "wc"]]
    /\ UNCHANGED <<state, generation, privateReady, schedulerReady,
                   dispatchAuthority, failedSeen, staleGenerationAttempted,
                   restartAttempted, panicRaised, initialPatSlot0,
                   patSlot0Validated, patReadback,
                   sharedMemoryWbMapped, sharedMemorySelector,
                   bspMemoryTypeBaselineCaptured, mtrrBaselineRestored,
                   cacheState>>

ReadbackPatKernelCacheContract(cpu) ==
    /\ state[cpu] = Starting
    /\ (cpu = Bsp \/ cacheState[cpu] = NoFillCache)
    /\ patProgrammed[cpu] = PatKernelCacheContract
    /\ patReadback[cpu] = PatUnread
    /\ patReadback' = [patReadback EXCEPT ![cpu] = PatKernelCacheContract]
    /\ UNCHANGED <<state, generation, privateReady, schedulerReady,
                   dispatchAuthority, failedSeen, staleGenerationAttempted,
                   restartAttempted, panicRaised, initialPatSlot0,
                   patSlot0Validated, patProgrammed,
                   sharedMemoryWbMapped, sharedMemorySelector,
                   bspMemoryTypeBaselineCaptured, mtrrBaselineRestored,
                   cacheState>>

CaptureBspMemoryTypeBaseline ==
    /\ state[Bsp] = Starting
    /\ cacheState[Bsp] = CacheEnabled
    /\ PatKernelCacheContractReady(Bsp)
    /\ ~bspMemoryTypeBaselineCaptured
    /\ bspMemoryTypeBaselineCaptured' = TRUE
    /\ mtrrBaselineRestored' = [mtrrBaselineRestored EXCEPT ![Bsp] = TRUE]
    /\ UNCHANGED <<state, generation, privateReady, schedulerReady,
                   dispatchAuthority, failedSeen, staleGenerationAttempted,
                   restartAttempted, panicRaised, initialPatSlot0,
                   patSlot0Validated, patProgrammed, patReadback,
                   sharedMemoryWbMapped, sharedMemorySelector, cacheState>>

RestoreApMemoryTypeBaseline(cpu) ==
    /\ cpu # Bsp
    /\ state[cpu] = Starting
    /\ cacheState[cpu] = NoFillCache
    /\ bspMemoryTypeBaselineCaptured
    /\ ~mtrrBaselineRestored[cpu]
    /\ mtrrBaselineRestored' = [mtrrBaselineRestored EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<state, generation, privateReady, schedulerReady,
                   dispatchAuthority, failedSeen, staleGenerationAttempted,
                   restartAttempted, panicRaised, initialPatSlot0,
                   patSlot0Validated, patProgrammed, patReadback,
                   sharedMemoryWbMapped, sharedMemorySelector,
                   bspMemoryTypeBaselineCaptured, cacheState>>

EnableApCache(cpu) ==
    /\ cpu # Bsp
    /\ state[cpu] = Starting
    /\ cacheState[cpu] = NoFillCache
    /\ mtrrBaselineRestored[cpu]
    /\ PatKernelCacheContractReady(cpu)
    /\ cacheState' = [cacheState EXCEPT ![cpu] = CacheEnabled]
    /\ UNCHANGED <<state, generation, privateReady, schedulerReady,
                   dispatchAuthority, failedSeen, staleGenerationAttempted,
                   restartAttempted, panicRaised, initialPatSlot0,
                   patSlot0Validated, patProgrammed, patReadback,
                   sharedMemoryWbMapped, sharedMemorySelector,
                   bspMemoryTypeBaselineCaptured, mtrrBaselineRestored>>

MapSharedMemoryWb ==
    /\ ~sharedMemoryWbMapped
    /\ sharedMemoryWbMapped' = TRUE
    /\ sharedMemorySelector' = WbSelector
    /\ UNCHANGED <<state, generation, privateReady, schedulerReady,
                   dispatchAuthority, failedSeen, staleGenerationAttempted,
                   restartAttempted, panicRaised, initialPatSlot0,
                   patSlot0Validated, patProgrammed, patReadback,
                   bspMemoryTypeBaselineCaptured, mtrrBaselineRestored,
                   cacheState>>

PublishPrivateState(cpu) ==
    /\ state[cpu] = Starting
    /\ generation[cpu] = 1
    /\ CpuMemoryTypeContractReady(cpu)
    /\ state' = [state EXCEPT ![cpu] = OnlineParked]
    /\ privateReady' = [privateReady EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<generation, schedulerReady, dispatchAuthority, failedSeen,
                   staleGenerationAttempted, restartAttempted, panicRaised,
                   initialPatSlot0, patSlot0Validated, patProgrammed,
                   patReadback, sharedMemoryWbMapped,
                   sharedMemorySelector, bspMemoryTypeBaselineCaptured,
                   mtrrBaselineRestored, cacheState>>

PublishScheduler(cpu) ==
    /\ state[cpu] = OnlineParked
    /\ privateReady[cpu]
    /\ state' = [state EXCEPT ![cpu] = SchedulerReady]
    /\ schedulerReady' = [schedulerReady EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<generation, privateReady, dispatchAuthority, failedSeen,
                   staleGenerationAttempted, restartAttempted, panicRaised,
                   initialPatSlot0, patSlot0Validated, patProgrammed,
                   patReadback, sharedMemoryWbMapped,
                   sharedMemorySelector, bspMemoryTypeBaselineCaptured,
                   mtrrBaselineRestored, cacheState>>

AdmitDispatch(cpu) ==
    /\ state[cpu] = SchedulerReady
    /\ privateReady[cpu]
    /\ schedulerReady[cpu]
    /\ CpuMemoryTypeContractReady(cpu)
    /\ state' = [state EXCEPT ![cpu] = Online]
    /\ dispatchAuthority' = [dispatchAuthority EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<generation, privateReady, schedulerReady, failedSeen,
                   staleGenerationAttempted, restartAttempted, panicRaised,
                   initialPatSlot0, patSlot0Validated, patProgrammed,
                   patReadback, sharedMemoryWbMapped,
                   sharedMemorySelector, bspMemoryTypeBaselineCaptured,
                   mtrrBaselineRestored, cacheState>>

Quarantine(cpu) ==
    /\ state[cpu] = Online
    /\ state' = [state EXCEPT ![cpu] = Quarantined]
    /\ dispatchAuthority' = [dispatchAuthority EXCEPT ![cpu] = FALSE]
    /\ UNCHANGED <<generation, privateReady, schedulerReady, failedSeen,
                   staleGenerationAttempted, restartAttempted, panicRaised,
                   initialPatSlot0, patSlot0Validated, patProgrammed,
                   patReadback, sharedMemoryWbMapped,
                   sharedMemorySelector, bspMemoryTypeBaselineCaptured,
                   mtrrBaselineRestored, cacheState>>

Fail(cpu) ==
    /\ state[cpu] \in {Starting, OnlineParked, SchedulerReady, Quarantined}
    /\ state' = [state EXCEPT ![cpu] = Failed]
    /\ dispatchAuthority' = [dispatchAuthority EXCEPT ![cpu] = FALSE]
    /\ failedSeen' = [failedSeen EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<generation, privateReady, schedulerReady,
                   staleGenerationAttempted, restartAttempted, panicRaised,
                   initialPatSlot0, patSlot0Validated, patProgrammed,
                   patReadback, sharedMemoryWbMapped,
                   sharedMemorySelector, bspMemoryTypeBaselineCaptured,
                   mtrrBaselineRestored, cacheState>>

RejectStaleGeneration ==
    /\ staleGenerationAttempted' = TRUE
    /\ panicRaised' = TRUE
    /\ UNCHANGED <<state, generation, privateReady, schedulerReady,
                   dispatchAuthority, failedSeen, restartAttempted,
                   initialPatSlot0, patSlot0Validated, patProgrammed,
                   patReadback, sharedMemoryWbMapped,
                   sharedMemorySelector, bspMemoryTypeBaselineCaptured,
                   mtrrBaselineRestored, cacheState>>

RejectRestart(cpu) ==
    /\ state[cpu] = Failed
    /\ restartAttempted' = TRUE
    /\ panicRaised' = TRUE
    /\ UNCHANGED <<state, generation, privateReady, schedulerReady,
                   dispatchAuthority, failedSeen, staleGenerationAttempted,
                   initialPatSlot0, patSlot0Validated, patProgrammed,
                   patReadback, sharedMemoryWbMapped,
                   sharedMemorySelector, bspMemoryTypeBaselineCaptured,
                   mtrrBaselineRestored, cacheState>>

Terminal ==
    /\ \A cpu \in Cpus : state[cpu] = Failed
    /\ UNCHANGED vars

PanicTerminal ==
    /\ panicRaised
    /\ UNCHANGED vars

Next ==
    IF panicRaised
    THEN PanicTerminal
    ELSE
        \/ \E cpu \in Cpus:
            Start(cpu)
            \/ EnterApNoFillCache(cpu)
            \/ ValidatePatSlot0Wb(cpu)
            \/ RejectInvalidPatSlot0(cpu)
            \/ ProgramPatKernelCacheContract(cpu)
            \/ ReadbackPatKernelCacheContract(cpu)
            \/ RestoreApMemoryTypeBaseline(cpu)
            \/ EnableApCache(cpu)
            \/ PublishPrivateState(cpu)
            \/ PublishScheduler(cpu)
            \/ AdmitDispatch(cpu)
            \/ Quarantine(cpu)
            \/ Fail(cpu)
            \/ RejectRestart(cpu)
        \/ MapSharedMemoryWb
        \/ CaptureBspMemoryTypeBaseline
        \/ RejectStaleGeneration
        \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ Bsp \in Cpus
    /\ state \in [Cpus ->
        {Discovered, Starting, OnlineParked, SchedulerReady, Online,
         Quarantined, Failed}]
    /\ generation \in [Cpus -> Nat]
    /\ privateReady \in [Cpus -> BOOLEAN]
    /\ schedulerReady \in [Cpus -> BOOLEAN]
    /\ dispatchAuthority \in [Cpus -> BOOLEAN]
    /\ failedSeen \in [Cpus -> BOOLEAN]
    /\ staleGenerationAttempted \in BOOLEAN
    /\ restartAttempted \in BOOLEAN
    /\ panicRaised \in BOOLEAN
    /\ initialPatSlot0 \in [Cpus -> {"wb", "non-wb"}]
    /\ patSlot0Validated \in [Cpus -> BOOLEAN]
    /\ patProgrammed \in [Cpus -> PatStates]
    /\ patReadback \in [Cpus -> PatStates]
    /\ sharedMemoryWbMapped \in BOOLEAN
    /\ sharedMemorySelector \in CacheSelectors
    /\ bspMemoryTypeBaselineCaptured \in BOOLEAN
    /\ mtrrBaselineRestored \in [Cpus -> BOOLEAN]
    /\ cacheState \in [Cpus -> CacheStates]

BootEpochGenerationIsFixed ==
    \A cpu \in Cpus : generation[cpu] = 1

ExactPatReadbackRequiresWbUcWcSelectors ==
    \A cpu \in Cpus:
        patReadback[cpu] # PatUnread =>
            patReadback[cpu] = PatKernelCacheContract
            /\ patReadback[cpu] = patProgrammed[cpu]

PatSlot0IsValidatedWithoutRetyping ==
    \A cpu \in Cpus:
        patSlot0Validated[cpu] =>
            /\ initialPatSlot0[cpu] = "wb"
            /\ patProgrammed[cpu].slot0 = initialPatSlot0[cpu]

InvalidPatSlot0CannotReachPrivateReady ==
    \A cpu \in Cpus:
        initialPatSlot0[cpu] # "wb" =>
            ~privateReady[cpu] /\ ~dispatchAuthority[cpu]

BspAndEveryApReadBackExactPatBeforePrivateReady ==
    \A cpu \in Cpus:
        state[cpu] \in PatRequiredStates =>
            CpuMemoryTypeContractReady(cpu) /\ privateReady[cpu]

PrivateReadyRequiresExactPatReadback ==
    \A cpu \in Cpus: privateReady[cpu] => CpuMemoryTypeContractReady(cpu)

BspBaselinePrecedesEveryApRestore ==
    \A cpu \in Cpus: mtrrBaselineRestored[cpu] => bspMemoryTypeBaselineCaptured

ResetOrNoFillApCannotPublishPrivateReady ==
    \A cpu \in Cpus:
        cacheState[cpu] # CacheEnabled =>
            ~privateReady[cpu] /\ ~dispatchAuthority[cpu]

ApCacheEnableRequiresExactRestoredMemoryTypes ==
    \A cpu \in Cpus:
        cpu # Bsp /\ cacheState[cpu] = CacheEnabled =>
            /\ mtrrBaselineRestored[cpu]
            /\ PatKernelCacheContractReady(cpu)

WbSharedMemoryDoesNotUseWcSelector ==
    sharedMemoryWbMapped => sharedMemorySelector = WbSelector

DispatchRequiresCompleteCpu ==
    \A cpu \in Cpus:
        dispatchAuthority[cpu] =>
            /\ state[cpu] = Online
            /\ privateReady[cpu]
            /\ schedulerReady[cpu]
            /\ CpuMemoryTypeContractReady(cpu)

OnlineRequiresCompleteCpu ==
    \A cpu \in Cpus:
        state[cpu] = Online =>
            /\ privateReady[cpu]
            /\ schedulerReady[cpu]
            /\ dispatchAuthority[cpu]
            /\ CpuMemoryTypeContractReady(cpu)

FailedCpuOwnsNoDispatch ==
    \A cpu \in Cpus : state[cpu] = Failed => ~dispatchAuthority[cpu]

FailedStateIsTerminalForBoot ==
    \A cpu \in Cpus : failedSeen[cpu] => state[cpu] = Failed

InvalidGenerationFailsClosed == staleGenerationAttempted => panicRaised

RestartFailsClosed == restartAttempted => panicRaised

=============================================================================
