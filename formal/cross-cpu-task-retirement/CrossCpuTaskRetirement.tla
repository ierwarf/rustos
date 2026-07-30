-------------------- MODULE CrossCpuTaskRetirement --------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Models the exec/exit barrier between scheduler ownership, process attachment,
address-space activation, and final page-table reclaim.

Concrete owners:
  * kernel/ps/src/multitask/{current,process_table}.rs
  * kernel/ps/src/multitask/scheduler/{smp.rs}
  * kernel/hal/src/arch/tlb_shootdown.rs
***************************************************************************)

CONSTANT Cpus

Tasks == {"target", "sibling"}
NoneCpu == 2
Idle == "idle"
Sealed == "sealed"
Quiescing == "quiescing"
Replacing == "replacing"
Flushing == "flushing"
Reclaimed == "reclaimed"
Panicked == "panicked"

VARIABLES phase, owner, targetQuiesced, siblingRetireRequested,
          siblingAttached, processGeneration, targetGeneration,
          tlbTargets, tlbAcks

vars == <<phase, owner, targetQuiesced, siblingRetireRequested,
          siblingAttached, processGeneration, targetGeneration,
          tlbTargets, tlbAcks>>

UniqueOwners(mapping) ==
    \A left, right \in Tasks:
        /\ left # right
        /\ mapping[left] # NoneCpu
        => mapping[left] # mapping[right]

ActiveCpus(mapping) == {cpu \in Cpus: \E task \in Tasks: mapping[task] = cpu}

Init ==
    /\ phase = Idle
    /\ owner \in [Tasks -> Cpus \cup {NoneCpu}]
    /\ UniqueOwners(owner)
    /\ targetQuiesced = FALSE
    /\ siblingRetireRequested = FALSE
    /\ siblingAttached = TRUE
    /\ processGeneration = 1
    /\ targetGeneration = 1
    /\ tlbTargets = {}
    /\ tlbAcks = {}

BeginExec ==
    /\ phase = Idle
    /\ phase' = Sealed
    /\ UNCHANGED <<owner, targetQuiesced, siblingRetireRequested,
                   siblingAttached, processGeneration, targetGeneration,
                   tlbTargets, tlbAcks>>

RequestQuiescence ==
    /\ phase = Sealed
    /\ phase' = Quiescing
    /\ targetQuiesced' = TRUE
    /\ siblingRetireRequested' = TRUE
    /\ UNCHANGED <<owner, siblingAttached, processGeneration,
                   targetGeneration, tlbTargets, tlbAcks>>

LeaveCpu(task) ==
    /\ phase = Quiescing
    /\ task \in Tasks
    /\ owner[task] # NoneCpu
    /\ IF task = "target" THEN targetQuiesced ELSE siblingRetireRequested
    /\ owner' = [owner EXCEPT ![task] = NoneCpu]
    /\ UNCHANGED <<phase, targetQuiesced, siblingRetireRequested,
                   siblingAttached, processGeneration, targetGeneration,
                   tlbTargets, tlbAcks>>

DetachSibling ==
    /\ phase = Quiescing
    /\ siblingRetireRequested
    /\ owner["sibling"] = NoneCpu
    /\ siblingAttached
    /\ siblingAttached' = FALSE
    /\ UNCHANGED <<phase, owner, targetQuiesced, siblingRetireRequested,
                   processGeneration, targetGeneration, tlbTargets, tlbAcks>>

Replace ==
    /\ phase = Quiescing
    /\ targetQuiesced
    /\ owner["target"] = NoneCpu
    /\ ~siblingAttached
    /\ phase' = Replacing
    /\ processGeneration' = processGeneration + 1
    /\ targetGeneration' = processGeneration + 1
    /\ targetQuiesced' = FALSE
    /\ UNCHANGED <<owner, siblingRetireRequested, siblingAttached,
                   tlbTargets, tlbAcks>>

PublishFinalShootdown ==
    /\ phase = Replacing
    /\ phase' = Flushing
    /\ tlbTargets' = Cpus
    /\ tlbAcks' = {}
    /\ UNCHANGED <<owner, targetQuiesced, siblingRetireRequested,
                   siblingAttached, processGeneration, targetGeneration>>

Acknowledge(cpu) ==
    /\ phase = Flushing
    /\ cpu \in tlbTargets
    /\ tlbAcks' = tlbAcks \cup {cpu}
    /\ UNCHANGED <<phase, owner, targetQuiesced, siblingRetireRequested,
                   siblingAttached, processGeneration, targetGeneration,
                   tlbTargets>>

Reclaim ==
    /\ phase = Flushing
    /\ tlbAcks = tlbTargets
    /\ ActiveCpus(owner) = {}
    /\ phase' = Reclaimed
    /\ UNCHANGED <<owner, targetQuiesced, siblingRetireRequested,
                   siblingAttached, processGeneration, targetGeneration,
                   tlbTargets, tlbAcks>>

Timeout ==
    /\ phase \in {Sealed, Quiescing, Flushing}
    /\ phase' = Panicked
    /\ UNCHANGED <<owner, targetQuiesced, siblingRetireRequested,
                   siblingAttached, processGeneration, targetGeneration,
                   tlbTargets, tlbAcks>>

Terminal ==
    /\ phase \in {Reclaimed, Panicked}
    /\ UNCHANGED vars

Next ==
    \/ BeginExec
    \/ RequestQuiescence
    \/ \E task \in Tasks: LeaveCpu(task)
    \/ DetachSibling
    \/ Replace
    \/ PublishFinalShootdown
    \/ \E cpu \in Cpus: Acknowledge(cpu)
    \/ Reclaim
    \/ Timeout
    \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {Idle, Sealed, Quiescing, Replacing, Flushing,
                  Reclaimed, Panicked}
    /\ owner \in [Tasks -> Cpus \cup {NoneCpu}]
    /\ targetQuiesced \in BOOLEAN
    /\ siblingRetireRequested \in BOOLEAN
    /\ siblingAttached \in BOOLEAN
    /\ processGeneration \in Nat
    /\ targetGeneration \in Nat
    /\ tlbTargets \subseteq Cpus
    /\ tlbAcks \subseteq Cpus

UniqueRunningOwnership == UniqueOwners(owner)

SiblingCleanupRequiresQuiescence ==
    ~siblingAttached => owner["sibling"] = NoneCpu

ReplacementRequiresAllOldExecutionStopped ==
    phase \in {Replacing, Flushing, Reclaimed} =>
        /\ owner["target"] = NoneCpu
        /\ ~siblingAttached
        /\ targetGeneration = processGeneration

ReclaimRequiresNoOwnerAndExactAcks ==
    phase = Reclaimed =>
        /\ ActiveCpus(owner) = {}
        /\ tlbAcks = tlbTargets

=============================================================================
