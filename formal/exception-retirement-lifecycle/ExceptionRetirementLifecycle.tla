---------------------- MODULE ExceptionRetirementLifecycle ----------------------
EXTENDS Naturals

(*******************************************************************************
Models x86 exception entry through the alignment bridge, scalar-only
diagnostics, ring classification, and the terminal resume/retirement decision.
It also admits an NMI while an arbitrary tracked lock is owned: the dedicated
IST emergency leaf returns without probing memory or acquiring any lock.

Concrete owners:
  * kernel/hal/src/arch/{gdt.rs,idt/mod.rs,idt/handlers.rs}
  * kernel/executive/src/hal_hooks.rs
  * kernel/compat/src/user/syscall/mod.rs
  * kernel/ps/src/multitask/scheduler.rs

The saved user RSP is an untrusted scalar. Its mappedness is deliberately
nondeterministic for user faults, yet diagnostics never dereference it and
therefore cannot create a nested kernel fault. NMI records only the fixed
emergency marker even when it interrupted a lock owner.
*******************************************************************************)

Idle == "idle"
Raw == "raw"
Aligned == "aligned"
Diagnosed == "diagnosed"
Classified == "classified"
Resumed == "resumed"
ThreadRetired == "thread-retired"
ProcessRetired == "process-retired"
KernelPanicked == "kernel-panicked"
NmiEntered == "nmi-entered"
NmiReturned == "nmi-returned"

NoneKind == "none"
RecoverableUser == "recoverable-user"
FatalUser == "fatal-user"
KernelFault == "kernel-fault"
NmiKind == "nmi"

VARIABLES phase,
          faultKind,
          stackAligned,
          diagnosticMemoryValid,
          diagnosticProbeAttempted,
          nestedFault,
          interruptedLockHeld,
          nmiTookLock,
          liveThreads,
          taskAuthority,
          processAuthority,
          endpointAuthority,
          waiterAuthority

vars == <<phase, faultKind, stackAligned, diagnosticMemoryValid,
          diagnosticProbeAttempted, nestedFault, interruptedLockHeld,
          nmiTookLock, liveThreads, taskAuthority, processAuthority,
          endpointAuthority, waiterAuthority>>

Init ==
    /\ phase = Idle
    /\ faultKind = NoneKind
    /\ stackAligned = FALSE
    /\ diagnosticMemoryValid = FALSE
    /\ diagnosticProbeAttempted = FALSE
    /\ nestedFault = FALSE
    /\ interruptedLockHeld = FALSE
    /\ nmiTookLock = FALSE
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
    /\ diagnosticMemoryValid' \in BOOLEAN
    /\ diagnosticProbeAttempted' = FALSE
    /\ nestedFault' = FALSE
    /\ interruptedLockHeld' = FALSE
    /\ nmiTookLock' = FALSE
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
    /\ diagnosticMemoryValid' \in BOOLEAN
    /\ diagnosticProbeAttempted' = FALSE
    /\ nestedFault' = FALSE
    /\ interruptedLockHeld' = FALSE
    /\ nmiTookLock' = FALSE
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
    /\ diagnosticMemoryValid' \in BOOLEAN
    /\ diagnosticProbeAttempted' = FALSE
    /\ nestedFault' = FALSE
    /\ interruptedLockHeld' = FALSE
    /\ nmiTookLock' = FALSE
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
    /\ diagnosticMemoryValid' = TRUE
    /\ diagnosticProbeAttempted' = FALSE
    /\ nestedFault' = FALSE
    /\ interruptedLockHeld' = FALSE
    /\ nmiTookLock' = FALSE
    /\ liveThreads' = 0
    /\ taskAuthority' = FALSE
    /\ processAuthority' = FALSE
    /\ endpointAuthority' = FALSE
    /\ waiterAuthority' = FALSE

RaiseNmi(lockHeld) ==
    /\ lockHeld \in BOOLEAN
    /\ phase = Idle
    /\ phase' = NmiEntered
    /\ faultKind' = NmiKind
    /\ stackAligned' = TRUE
    /\ diagnosticMemoryValid' = FALSE
    /\ diagnosticProbeAttempted' = FALSE
    /\ nestedFault' = FALSE
    /\ interruptedLockHeld' = lockHeld
    /\ nmiTookLock' = FALSE
    /\ liveThreads' = 0
    /\ taskAuthority' = FALSE
    /\ processAuthority' = FALSE
    /\ endpointAuthority' = FALSE
    /\ waiterAuthority' = FALSE

BridgeAlign ==
    /\ phase = Raw
    /\ phase' = Aligned
    /\ stackAligned' = TRUE
    /\ UNCHANGED <<faultKind, diagnosticMemoryValid, diagnosticProbeAttempted,
                    nestedFault, interruptedLockHeld, nmiTookLock, liveThreads,
                    taskAuthority, processAuthority, endpointAuthority,
                    waiterAuthority>>

RecordScalarDiagnostics ==
    /\ phase = Aligned
    /\ stackAligned
    /\ phase' = Diagnosed
    /\ diagnosticProbeAttempted' = FALSE
    /\ nestedFault' = FALSE
    /\ UNCHANGED <<faultKind, stackAligned, diagnosticMemoryValid,
                    interruptedLockHeld, nmiTookLock, liveThreads,
                    taskAuthority, processAuthority, endpointAuthority,
                    waiterAuthority>>

Classify ==
    /\ phase = Diagnosed
    /\ stackAligned
    /\ phase' = Classified
    /\ UNCHANGED <<faultKind, stackAligned, diagnosticMemoryValid,
                    diagnosticProbeAttempted, nestedFault, interruptedLockHeld,
                    nmiTookLock, liveThreads, taskAuthority, processAuthority,
                    endpointAuthority, waiterAuthority>>

ResumeUser ==
    /\ phase = Classified
    /\ faultKind = RecoverableUser
    /\ phase' = Resumed
    /\ UNCHANGED <<faultKind, stackAligned, diagnosticMemoryValid,
                    diagnosticProbeAttempted, nestedFault, interruptedLockHeld,
                    nmiTookLock, liveThreads, taskAuthority, processAuthority,
                    endpointAuthority, waiterAuthority>>

RetireNonFinal ==
    /\ phase = Classified
    /\ faultKind = FatalUser
    /\ liveThreads > 1
    /\ phase' = ThreadRetired
    /\ liveThreads' = liveThreads - 1
    /\ taskAuthority' = FALSE
    /\ waiterAuthority' = FALSE
    /\ UNCHANGED <<faultKind, stackAligned, diagnosticMemoryValid,
                    diagnosticProbeAttempted, nestedFault, interruptedLockHeld,
                    nmiTookLock, processAuthority, endpointAuthority>>

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
    /\ UNCHANGED <<faultKind, stackAligned, diagnosticMemoryValid,
                    diagnosticProbeAttempted, nestedFault, interruptedLockHeld,
                    nmiTookLock>>

PanicKernel ==
    /\ phase = Classified
    /\ faultKind = KernelFault
    /\ phase' = KernelPanicked
    /\ UNCHANGED <<faultKind, stackAligned, diagnosticMemoryValid,
                    diagnosticProbeAttempted, nestedFault, interruptedLockHeld,
                    nmiTookLock, liveThreads, taskAuthority, processAuthority,
                    endpointAuthority, waiterAuthority>>

ReturnNmi ==
    /\ phase = NmiEntered
    /\ phase' = NmiReturned
    /\ UNCHANGED <<faultKind, stackAligned, diagnosticMemoryValid,
                    diagnosticProbeAttempted, nestedFault, interruptedLockHeld,
                    nmiTookLock, liveThreads, taskAuthority, processAuthority,
                    endpointAuthority, waiterAuthority>>

ExceptionStep ==
    BridgeAlign \/ RecordScalarDiagnostics \/ Classify \/ ResumeUser
    \/ RetireNonFinal \/ RetireFinal \/ PanicKernel \/ ReturnNmi

Next ==
    \/ RaiseRecoverableUser
    \/ RaiseFatalNonFinal
    \/ RaiseFatalFinal
    \/ RaiseKernelFault
    \/ \E lockHeld \in BOOLEAN: RaiseNmi(lockHeld)
    \/ ExceptionStep

TypeOK ==
    /\ phase \in {Idle, Raw, Aligned, Diagnosed, Classified, Resumed,
                   ThreadRetired, ProcessRetired, KernelPanicked,
                   NmiEntered, NmiReturned}
    /\ faultKind \in {NoneKind, RecoverableUser, FatalUser, KernelFault, NmiKind}
    /\ stackAligned \in BOOLEAN
    /\ diagnosticMemoryValid \in BOOLEAN
    /\ diagnosticProbeAttempted \in BOOLEAN
    /\ nestedFault \in BOOLEAN
    /\ interruptedLockHeld \in BOOLEAN
    /\ nmiTookLock \in BOOLEAN
    /\ liveThreads \in 0..2
    /\ taskAuthority \in BOOLEAN
    /\ processAuthority \in BOOLEAN
    /\ endpointAuthority \in BOOLEAN
    /\ waiterAuthority \in BOOLEAN

RustCleanupRequiresAligned ==
    phase \in {Diagnosed, Classified, Resumed, ThreadRetired,
               ProcessRetired, KernelPanicked, NmiEntered, NmiReturned}
        => stackAligned

UserDiagnosticNeverProbesMemory ==
    faultKind \in {RecoverableUser, FatalUser} =>
        ~diagnosticProbeAttempted /\ ~nestedFault

NmiTakesNoTrackedLock ==
    phase \in {NmiEntered, NmiReturned} => ~nmiTookLock

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
    phase \in {Raw, Aligned, Diagnosed, Classified, NmiEntered} ~>
        phase \in {Resumed, ThreadRetired, ProcessRetired, KernelPanicked,
                   NmiReturned}

Spec == Init /\ [][Next]_vars /\ WF_vars(ExceptionStep)

=============================================================================
