---------------------- MODULE ExceptionRetirementLifecycle ----------------------
EXTENDS Naturals

(*******************************************************************************
Models the complete high-risk path from one x86 exception entry through the
alignment bridge, ring/user classification, and the terminal resume or
retirement decision.

Concrete owners:
  * kernel/hal/src/arch/idt/handlers.rs
  * kernel/executive/src/hal_hooks.rs
  * kernel/compat/src/user/syscall/mod.rs
  * kernel/ps/src/multitask/scheduler.rs

The generated `x86-interrupt` wrapper is not an ordinary SysV caller for every
error-code vector.  No Rust cleanup may therefore execute before the explicit
bridge establishes a 16-byte call-site stack.  A recoverable user fault keeps
all process authority.  A fatal non-final thread fault retires task-local
wait/reply authority only.  A fatal final-thread fault additionally revokes
the process endpoint and all process-owned IPC authority.  Kernel faults never
take the user-retirement route.

Linearization points: BridgeAlign establishes the Rust-call ABI; ResumeUser
commits recovery; RetireNonFinal removes the exact task; RetireFinal marks the
process exiting and revokes process authority in the same abstract step.
*******************************************************************************)

Idle == "idle"
Raw == "raw"
Aligned == "aligned"
Classified == "classified"
Resumed == "resumed"
ThreadRetired == "thread-retired"
ProcessRetired == "process-retired"
KernelPanicked == "kernel-panicked"

NoneKind == "none"
RecoverableUser == "recoverable-user"
FatalUser == "fatal-user"
KernelFault == "kernel-fault"

VARIABLES phase,
          faultKind,
          stackAligned,
          liveThreads,
          taskAuthority,
          processAuthority,
          endpointAuthority,
          waiterAuthority

vars == <<phase, faultKind, stackAligned, liveThreads, taskAuthority,
          processAuthority, endpointAuthority, waiterAuthority>>

Init ==
    /\ phase = Idle
    /\ faultKind = NoneKind
    /\ stackAligned = FALSE
    /\ liveThreads = 0
    /\ taskAuthority = FALSE
    /\ processAuthority = FALSE
    /\ endpointAuthority = FALSE
    /\ waiterAuthority = FALSE

RaiseRecoverableUser ==
    /\ phase = Idle
    /\ phase' = Raw
    /\ faultKind' = RecoverableUser
    /\ stackAligned' = FALSE
    /\ liveThreads' = 1
    /\ taskAuthority' = TRUE
    /\ processAuthority' = TRUE
    /\ endpointAuthority' = TRUE
    /\ waiterAuthority' = TRUE

RaiseFatalNonFinal ==
    /\ phase = Idle
    /\ phase' = Raw
    /\ faultKind' = FatalUser
    /\ stackAligned' = FALSE
    /\ liveThreads' = 2
    /\ taskAuthority' = TRUE
    /\ processAuthority' = TRUE
    /\ endpointAuthority' = TRUE
    /\ waiterAuthority' = TRUE

RaiseFatalFinal ==
    /\ phase = Idle
    /\ phase' = Raw
    /\ faultKind' = FatalUser
    /\ stackAligned' = FALSE
    /\ liveThreads' = 1
    /\ taskAuthority' = TRUE
    /\ processAuthority' = TRUE
    /\ endpointAuthority' = TRUE
    /\ waiterAuthority' = TRUE

RaiseKernelFault ==
    /\ phase = Idle
    /\ phase' = Raw
    /\ faultKind' = KernelFault
    /\ stackAligned' = FALSE
    /\ liveThreads' = 0
    /\ taskAuthority' = FALSE
    /\ processAuthority' = FALSE
    /\ endpointAuthority' = FALSE
    /\ waiterAuthority' = FALSE

BridgeAlign ==
    /\ phase = Raw
    /\ phase' = Aligned
    /\ stackAligned' = TRUE
    /\ UNCHANGED <<faultKind, liveThreads, taskAuthority, processAuthority,
                  endpointAuthority, waiterAuthority>>

Classify ==
    /\ phase = Aligned
    /\ stackAligned
    /\ phase' = Classified
    /\ UNCHANGED <<faultKind, stackAligned, liveThreads, taskAuthority,
                  processAuthority, endpointAuthority, waiterAuthority>>

ResumeUser ==
    /\ phase = Classified
    /\ faultKind = RecoverableUser
    /\ phase' = Resumed
    /\ UNCHANGED <<faultKind, stackAligned, liveThreads, taskAuthority,
                  processAuthority, endpointAuthority, waiterAuthority>>

RetireNonFinal ==
    /\ phase = Classified
    /\ faultKind = FatalUser
    /\ liveThreads > 1
    /\ phase' = ThreadRetired
    /\ liveThreads' = liveThreads - 1
    /\ taskAuthority' = FALSE
    /\ waiterAuthority' = FALSE
    /\ UNCHANGED <<faultKind, stackAligned, processAuthority,
                  endpointAuthority>>

RetireFinal ==
    /\ phase = Classified
    /\ faultKind = FatalUser
    /\ liveThreads = 1
    /\ phase' = ProcessRetired
    /\ liveThreads' = 0
    /\ taskAuthority' = FALSE
    /\ processAuthority' = FALSE
    /\ endpointAuthority' = FALSE
    /\ waiterAuthority' = FALSE
    /\ UNCHANGED <<faultKind, stackAligned>>

PanicKernel ==
    /\ phase = Classified
    /\ faultKind = KernelFault
    /\ phase' = KernelPanicked
    /\ UNCHANGED <<faultKind, stackAligned, liveThreads, taskAuthority,
                  processAuthority, endpointAuthority, waiterAuthority>>

Next ==
    \/ RaiseRecoverableUser
    \/ RaiseFatalNonFinal
    \/ RaiseFatalFinal
    \/ RaiseKernelFault
    \/ BridgeAlign
    \/ Classify
    \/ ResumeUser
    \/ RetireNonFinal
    \/ RetireFinal
    \/ PanicKernel

TypeOK ==
    /\ phase \in {Idle, Raw, Aligned, Classified, Resumed, ThreadRetired,
                  ProcessRetired, KernelPanicked}
    /\ faultKind \in {NoneKind, RecoverableUser, FatalUser, KernelFault}
    /\ stackAligned \in BOOLEAN
    /\ liveThreads \in 0..2
    /\ taskAuthority \in BOOLEAN
    /\ processAuthority \in BOOLEAN
    /\ endpointAuthority \in BOOLEAN
    /\ waiterAuthority \in BOOLEAN

RustCleanupRequiresAligned ==
    phase \in {Classified, Resumed, ThreadRetired, ProcessRetired,
               KernelPanicked} => stackAligned

RecoveredUserKeepsAuthority ==
    phase = Resumed =>
        taskAuthority /\ processAuthority /\ endpointAuthority /\ waiterAuthority

NonFinalRetirementKeepsProcessEndpoint ==
    phase = ThreadRetired =>
        liveThreads = 1 /\ ~taskAuthority /\ ~waiterAuthority /\
        processAuthority /\ endpointAuthority

FinalRetirementRevokesAllAuthority ==
    phase = ProcessRetired =>
        liveThreads = 0 /\ ~taskAuthority /\ ~waiterAuthority /\
        ~processAuthority /\ ~endpointAuthority

KernelFaultCannotRetireUserAuthority ==
    faultKind = KernelFault =>
        ~taskAuthority /\ ~processAuthority /\ ~endpointAuthority /\
        ~waiterAuthority

RetiredTaskHasNoWaiter ==
    phase \in {ThreadRetired, ProcessRetired} => ~waiterAuthority

RaisedExceptionEventuallySettles ==
    phase \in {Raw, Aligned, Classified} ~>
        phase \in {Resumed, ThreadRetired, ProcessRetired, KernelPanicked}

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

=============================================================================
