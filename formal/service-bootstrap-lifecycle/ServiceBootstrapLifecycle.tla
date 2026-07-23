---------------------- MODULE ServiceBootstrapLifecycle ----------------------
EXTENDS Naturals

(*******************************************************************************
Models the root supervisor entry, helper-thread handoff, and initd dependency
authorization path. Rust code is entered only through a stack-aligning raw
trampoline, a non-final helper exit preserves the process-owned endpoint, and
initd receives lookup authority for exactly the bootstrap dependencies declared
by rootd before it starts post-init services.

Concrete owners:
  * services/rootd/src/main.rs
  * kernel/compat/src/user/syscall/linux.rs
  * kernel/ps/src/multitask/scheduler.rs
*******************************************************************************)

TerminalPhases == {"entry-rejected", "ready", "denied"}

VARIABLES phase, entryAligned, processThreads, workerCompleted, endpointPublished,
          declaredDependenciesAuthorized, lookupOutcome

vars == <<phase, entryAligned, processThreads, workerCompleted, endpointPublished,
          declaredDependenciesAuthorized, lookupOutcome>>

Init ==
    /\ phase = "raw-entry"
    /\ entryAligned = FALSE
    /\ processThreads = 0
    /\ workerCompleted = FALSE
    /\ endpointPublished = FALSE
    /\ declaredDependenciesAuthorized = FALSE
    /\ lookupOutcome = "none"

AlignEntry ==
    /\ phase = "raw-entry"
    /\ phase' = "aligned"
    /\ entryAligned' = TRUE
    /\ UNCHANGED <<processThreads, workerCompleted, endpointPublished,
                   declaredDependenciesAuthorized, lookupOutcome>>

RejectMissingTrampoline ==
    /\ phase = "raw-entry"
    /\ phase' = "entry-rejected"
    /\ UNCHANGED <<entryAligned, processThreads, workerCompleted, endpointPublished,
                   declaredDependenciesAuthorized, lookupOutcome>>

EnterSupervisor ==
    /\ phase = "aligned"
    /\ entryAligned
    /\ phase' = "supervisor"
    /\ processThreads' = 1
    /\ endpointPublished' = TRUE
    /\ UNCHANGED <<entryAligned, workerCompleted,
                   declaredDependenciesAuthorized, lookupOutcome>>

CloneWorker ==
    /\ phase = "supervisor"
    /\ processThreads = 1
    /\ ~workerCompleted
    /\ phase' = "supervisor-worker"
    /\ processThreads' = 2
    /\ UNCHANGED <<entryAligned, workerCompleted, endpointPublished,
                   declaredDependenciesAuthorized, lookupOutcome>>

ExitNonfinalWorker ==
    /\ phase = "supervisor-worker"
    /\ processThreads = 2
    /\ endpointPublished
    /\ phase' = "supervisor"
    /\ processThreads' = 1
    /\ workerCompleted' = TRUE
    /\ UNCHANGED <<entryAligned, endpointPublished,
                   declaredDependenciesAuthorized, lookupOutcome>>

ActivateInitd ==
    /\ phase = "supervisor"
    /\ processThreads = 1
    /\ workerCompleted
    /\ endpointPublished
    /\ phase' = "initd-active"
    /\ UNCHANGED <<entryAligned, processThreads, workerCompleted, endpointPublished,
                   declaredDependenciesAuthorized, lookupOutcome>>

AuthorizeDeclaredDependencies ==
    /\ phase = "initd-active"
    /\ phase' = "initd-authorized"
    /\ declaredDependenciesAuthorized' = TRUE
    /\ UNCHANGED <<entryAligned, processThreads, workerCompleted,
                   endpointPublished, lookupOutcome>>

LookupDeclaredDependency ==
    /\ phase = "initd-authorized"
    /\ declaredDependenciesAuthorized
    /\ endpointPublished
    /\ phase' = "ready"
    /\ lookupOutcome' = "granted"
    /\ UNCHANGED <<entryAligned, processThreads, workerCompleted, endpointPublished,
                   declaredDependenciesAuthorized>>

DenyUndeclaredDependency ==
    /\ phase = "initd-authorized"
    /\ phase' = "denied"
    /\ lookupOutcome' = "denied"
    /\ UNCHANGED <<entryAligned, processThreads, workerCompleted, endpointPublished,
                   declaredDependenciesAuthorized>>

FailDeclaredLookupContract ==
    /\ phase = "initd-authorized"
    /\ phase' = "denied"
    /\ lookupOutcome' = "contract-error"
    /\ UNCHANGED <<entryAligned, processThreads, workerCompleted, endpointPublished,
                   declaredDependenciesAuthorized>>

Next ==
    \/ AlignEntry
    \/ RejectMissingTrampoline
    \/ EnterSupervisor
    \/ CloneWorker
    \/ ExitNonfinalWorker
    \/ ActivateInitd
    \/ AuthorizeDeclaredDependencies
    \/ LookupDeclaredDependency
    \/ DenyUndeclaredDependency
    \/ FailDeclaredLookupContract

TypeOK ==
    /\ phase \in {"raw-entry", "aligned", "supervisor", "supervisor-worker",
                   "initd-active", "initd-authorized"} \cup TerminalPhases
    /\ entryAligned \in BOOLEAN
    /\ processThreads \in 0..2
    /\ workerCompleted \in BOOLEAN
    /\ endpointPublished \in BOOLEAN
    /\ declaredDependenciesAuthorized \in BOOLEAN
    /\ lookupOutcome \in {"none", "granted", "denied", "contract-error"}

RustSupervisorRequiresAlignedEntry ==
    phase \in {"supervisor", "supervisor-worker", "initd-active",
               "initd-authorized", "ready", "denied"} => entryAligned

NonfinalWorkerExitKeepsEndpoint ==
    phase \in {"supervisor", "supervisor-worker", "initd-active",
               "initd-authorized", "ready", "denied"} => endpointPublished

GrantedLookupRequiresDeclaredAuthority ==
    lookupOutcome = "granted" => declaredDependenciesAuthorized

UndeclaredLookupNeverBecomesReady ==
    lookupOutcome = "denied" => phase = "denied"

LookupContractFailureNeverBecomesPending ==
    lookupOutcome = "contract-error" => phase = "denied"

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

EventuallyTerminal == <> (phase \in TerminalPhases)
=============================================================================
