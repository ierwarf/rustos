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
  * kernel/ps/src/multitask/scheduler/bootstrap{.rs,/initial_tasks.rs}
  * kernel/ps/src/multitask/scheduler/{kernel_creation,return_architecture}.rs
***************************************************************************)

CONSTANTS Cpus, Bsp
InitialNoCpu == 2 \* Outside this registered 0/1 bounded configuration.

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
          mtrrBaselineRestored, cacheState,
          imagePhase, initialPhase, initialProducer, initialOwner,
          physicalCurrent, uidCurrent, initialArchitecture

vars == <<state, generation, privateReady, schedulerReady, dispatchAuthority,
          failedSeen, staleGenerationAttempted, restartAttempted, panicRaised,
          initialPatSlot0, patSlot0Validated, patProgrammed, patReadback,
          sharedMemoryWbMapped,
          sharedMemorySelector, bspMemoryTypeBaselineCaptured,
          mtrrBaselineRestored, cacheState,
          imagePhase, initialPhase, initialProducer, initialOwner,
          physicalCurrent, uidCurrent, initialArchitecture>>

initialVars == <<imagePhase, initialPhase, initialProducer, initialOwner,
                 physicalCurrent, uidCurrent, initialArchitecture>>
baseVars == <<state, generation, privateReady, schedulerReady, dispatchAuthority,
          failedSeen, staleGenerationAttempted, restartAttempted, panicRaised,
          initialPatSlot0, patSlot0Validated, patProgrammed, patReadback,
          sharedMemoryWbMapped, sharedMemorySelector, bspMemoryTypeBaselineCaptured,
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
    /\ imagePhase = "vacant"
    /\ initialPhase = [cpu \in Cpus |-> 0]
    /\ initialProducer = [cpu \in Cpus |-> FALSE]
    /\ initialOwner = [cpu \in Cpus |-> InitialNoCpu]
    /\ physicalCurrent = [cpu \in Cpus |-> FALSE]
    /\ uidCurrent = [cpu \in Cpus |-> FALSE]
    /\ initialArchitecture = [cpu \in Cpus |-> FALSE]

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
    /\ imagePhase = "ready"
    /\ physicalCurrent[cpu] /\ uidCurrent[cpu]
    /\ state' = [state EXCEPT ![cpu] = SchedulerReady]
    /\ schedulerReady' = [schedulerReady EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<generation, privateReady, dispatchAuthority, failedSeen,
                   staleGenerationAttempted, restartAttempted, panicRaised,
                   initialPatSlot0, patSlot0Validated, patProgrammed,
                   patReadback, sharedMemoryWbMapped,
                   sharedMemorySelector, bspMemoryTypeBaselineCaptured,
                   mtrrBaselineRestored, cacheState>>

AdmitDispatch(cpu) ==
    /\ initialArchitecture[cpu]
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

BaseNext ==
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

(***************************************************************************
Bounded initial source join: bootstrap InitialCpu -> shared Ticket descriptor ->
Blocked producer ACK -> image-qualified Running -> original initial interval ->
physical allocation bank -> UID bank -> logical Current -> initial architecture.
The original producer overlaps both banks. AP remains on its live HAL RSP;
only BSP has a constructed frame. Earlier HAL memory-type inputs remain modeled.
This is NOT full selected funding/frame/MM/RSP/terminal/reuse refinement.
***************************************************************************)
BeginImage ==
    /\ imagePhase = "vacant"
    /\ \A cpu \in Cpus : state[cpu] = OnlineParked
    /\ imagePhase' = "building"
    /\ UNCHANGED <<initialPhase, initialProducer, initialOwner,
                   physicalCurrent, uidCurrent, initialArchitecture, baseVars>>

ReserveInitial(cpu) ==
    /\ imagePhase = "building" /\ state[cpu] = OnlineParked
    /\ initialPhase[cpu] = 0
    /\ initialPhase' = [initialPhase EXCEPT ![cpu] = 1]
    /\ initialProducer' = [initialProducer EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<imagePhase, initialOwner, physicalCurrent, uidCurrent,
                   initialArchitecture, baseVars>>

MaterializeInitial(cpu) ==
    /\ imagePhase = "building" /\ initialPhase[cpu] = 1
    /\ initialProducer[cpu]
    /\ initialPhase' = [initialPhase EXCEPT ![cpu] = 2]
    /\ UNCHANGED <<imagePhase, initialProducer, initialOwner,
                   physicalCurrent, uidCurrent, initialArchitecture, baseVars>>

BlockedInitialAck(cpu) ==
    /\ imagePhase = "building" /\ initialPhase[cpu] = 2
    /\ initialProducer[cpu]
    /\ initialPhase' = [initialPhase EXCEPT ![cpu] = 3]
    /\ UNCHANGED <<imagePhase, initialProducer, initialOwner,
                   physicalCurrent, uidCurrent, initialArchitecture, baseVars>>

AdmitInitialRunning(cpu) ==
    /\ imagePhase = "building" /\ state[cpu] = OnlineParked
    /\ initialPhase[cpu] = 3 /\ initialProducer[cpu]
    /\ initialPhase' = [initialPhase EXCEPT ![cpu] = 4]
    /\ initialOwner' = [initialOwner EXCEPT ![cpu] = cpu]
    /\ UNCHANGED <<imagePhase, initialProducer, physicalCurrent,
                   uidCurrent, initialArchitecture, baseVars>>

OpenInitialInterval(cpu) ==
    /\ imagePhase = "building" /\ initialPhase[cpu] = 4
    /\ initialProducer[cpu] /\ initialOwner[cpu] = cpu
    /\ initialPhase' = [initialPhase EXCEPT ![cpu] = 5]
    /\ UNCHANGED <<imagePhase, initialProducer, initialOwner,
                   physicalCurrent, uidCurrent, initialArchitecture, baseVars>>

PublishPhysicalCurrent(cpu) ==
    /\ imagePhase = "building" /\ initialPhase[cpu] = 5
    /\ initialProducer[cpu] /\ initialOwner[cpu] = cpu
    /\ initialPhase' = [initialPhase EXCEPT ![cpu] = 6]
    /\ physicalCurrent' = [physicalCurrent EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<imagePhase, initialProducer, initialOwner,
                   uidCurrent, initialArchitecture, baseVars>>

PublishUidCurrent(cpu) ==
    /\ imagePhase = "building" /\ initialPhase[cpu] = 6
    /\ initialProducer[cpu] /\ physicalCurrent[cpu]
    /\ initialPhase' = [initialPhase EXCEPT ![cpu] = 7]
    /\ uidCurrent' = [uidCurrent EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<imagePhase, initialProducer, initialOwner,
                   physicalCurrent, initialArchitecture, baseVars>>

PublishInitialCurrent(cpu) ==
    /\ imagePhase = "building" /\ initialPhase[cpu] = 7
    /\ initialProducer[cpu] /\ physicalCurrent[cpu] /\ uidCurrent[cpu]
    /\ initialPhase' = [initialPhase EXCEPT ![cpu] = 8]
    /\ UNCHANGED <<imagePhase, initialProducer, initialOwner,
                   physicalCurrent, uidCurrent, initialArchitecture, baseVars>>

ReleaseInitialProducer(cpu) ==
    /\ initialPhase[cpu] = 8 /\ initialProducer[cpu]
    /\ physicalCurrent[cpu] /\ uidCurrent[cpu]
    /\ initialProducer' = [initialProducer EXCEPT ![cpu] = FALSE]
    /\ UNCHANGED <<imagePhase, initialPhase, initialOwner,
                   physicalCurrent, uidCurrent, initialArchitecture, baseVars>>

PrepareInitialArchitecture(cpu) ==
    /\ initialPhase[cpu] = 8 /\ physicalCurrent[cpu] /\ uidCurrent[cpu]
    /\ initialOwner[cpu] = cpu /\ ~initialArchitecture[cpu]
    /\ IF cpu = Bsp THEN imagePhase = "building" /\ state[cpu] = OnlineParked
       ELSE imagePhase = "ready" /\ state[cpu] = SchedulerReady
    /\ initialArchitecture' = [initialArchitecture EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<imagePhase, initialPhase, initialProducer, initialOwner,
                   physicalCurrent, uidCurrent, baseVars>>

SealInitialImage ==
    /\ imagePhase = "building" /\ initialArchitecture[Bsp]
    /\ \A cpu \in Cpus : state[cpu] = OnlineParked /\ initialPhase[cpu] = 8
                          /\ ~initialProducer[cpu]
    /\ imagePhase' = "ready"
    /\ UNCHANGED <<initialPhase, initialProducer, initialOwner,
                   physicalCurrent, uidCurrent, initialArchitecture, baseVars>>

PoisonInitialImage ==
    /\ imagePhase = "building"
    /\ \E cpu \in Cpus : state[cpu] = Failed
    /\ imagePhase' = "poison"
    /\ UNCHANGED <<initialPhase, initialProducer, initialOwner,
                   physicalCurrent, uidCurrent, initialArchitecture, baseVars>>

Next ==
    \/ BaseNext /\ UNCHANGED initialVars
    \/ ~panicRaised /\ (BeginImage \/ SealInitialImage \/ PoisonInitialImage
        \/ \E cpu \in Cpus : ReserveInitial(cpu) \/ MaterializeInitial(cpu)
           \/ BlockedInitialAck(cpu) \/ AdmitInitialRunning(cpu)
           \/ OpenInitialInterval(cpu) \/ PublishPhysicalCurrent(cpu)
           \/ PublishUidCurrent(cpu) \/ PublishInitialCurrent(cpu)
           \/ ReleaseInitialProducer(cpu) \/ PrepareInitialArchitecture(cpu))

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
    /\ imagePhase \in {"vacant", "building", "ready", "poison"}
    /\ initialPhase \in [Cpus -> 0..8]
    /\ initialProducer \in [Cpus -> BOOLEAN]
    /\ InitialNoCpu \notin Cpus
    /\ initialOwner \in [Cpus -> Cpus \cup {InitialNoCpu}]
    /\ physicalCurrent \in [Cpus -> BOOLEAN]
    /\ uidCurrent \in [Cpus -> BOOLEAN]
    /\ initialArchitecture \in [Cpus -> BOOLEAN]

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

InitialRunningIsCpuExact ==
    \A cpu \in Cpus : initialPhase[cpu] >= 4 => initialOwner[cpu] = cpu
InitialBanksPrecedeLogicalCurrent ==
    \A cpu \in Cpus :
        /\ (initialPhase[cpu] >= 6 => physicalCurrent[cpu])
        /\ (initialPhase[cpu] >= 7 => uidCurrent[cpu])
OriginalProducerOverlapsBanks ==
    \A cpu \in Cpus : initialPhase[cpu] \in 1..7 => initialProducer[cpu]
InitialArchitectureRetainsBothBanks ==
    \A cpu \in Cpus : initialArchitecture[cpu] =>
        initialPhase[cpu] = 8 /\ physicalCurrent[cpu] /\ uidCurrent[cpu]
ReadyImageIncludesEveryCpuAndBspArchitecture ==
    imagePhase = "ready" => initialArchitecture[Bsp]
        /\ \A cpu \in Cpus : initialPhase[cpu] = 8 /\ ~initialProducer[cpu]
DispatchRequiresOriginalInitialArchitecture ==
    \A cpu \in Cpus : dispatchAuthority[cpu] =>
        imagePhase = "ready" /\ initialArchitecture[cpu]

=============================================================================
