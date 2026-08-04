-------------------- MODULE TlbShootdownLifecycle --------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Models address-space reference ownership, activation, serialized page-table
mutation, active-root-scoped target publication, exact-generation
acknowledgement, and terminal root reclamation.

The scheduler/process layer must release every task/slot/process reference and
deactivate the root before retirement starts. Once a root is Retiring, neither
a new reference nor a future Activate action is admitted. This makes the
otherwise implicit Rust ownership premise part of the checked refinement.

Concrete owners:
  * kernel/hal/src/arch/tlb_shootdown.rs
  * kernel/mm/src/memory/{address_space,kernel_vm}.rs
  * kernel/ps/src/multitask/{scheduler,process_table}.rs
***************************************************************************)

CONSTANTS Cpus, Roots, MaxRefs

NoneRoot == "none"
GlobalRoot == "global"
Idle == "idle"
Editing == "editing"
Published == "published"
Complete == "complete"

RootLive == "root-live"
RootRetiring == "root-retiring"
RootReclaimed == "root-reclaimed"

NoMutation == "no-mutation"
EditMutation == "edit-mutation"
GlobalMutation == "global-mutation"
RetireMutation == "retire-mutation"

VARIABLES activeRoot, eligible, rootState, references, phase, mutationKind,
          mutationRoot, generation, targets, requestGeneration, ackGeneration,
          lastActivationSameRoot, lastActivationReloaded, targetSnapshotRoot

vars == <<activeRoot, eligible, rootState, references, phase, mutationKind,
          mutationRoot, generation, targets, requestGeneration, ackGeneration,
          lastActivationSameRoot, lastActivationReloaded, targetSnapshotRoot>>

Init ==
    /\ activeRoot = [cpu \in Cpus |-> NoneRoot]
    /\ eligible = [cpu \in Cpus |-> FALSE]
    /\ rootState = [root \in Roots |-> RootLive]
    /\ references = [root \in Roots |-> 1]
    /\ phase = Idle
    /\ mutationKind = NoMutation
    /\ mutationRoot = NoneRoot
    /\ generation = 0
    /\ targets = {}
    /\ requestGeneration = [cpu \in Cpus |-> 0]
    /\ ackGeneration = [cpu \in Cpus |-> 0]
    /\ lastActivationSameRoot = FALSE
    /\ lastActivationReloaded = FALSE
    /\ targetSnapshotRoot = [cpu \in Cpus |-> NoneRoot]

EligibleTargets == {cpu \in Cpus: eligible[cpu]}
AddressSpaceTargets(root) ==
    {cpu \in Cpus: eligible[cpu] /\ activeRoot[cpu] = root}

AcquireReference(root) ==
    /\ rootState[root] = RootLive
    /\ references[root] < MaxRefs
    /\ references' = [references EXCEPT ![root] = @ + 1]
    /\ UNCHANGED <<activeRoot, eligible, rootState, phase, mutationKind,
                   mutationRoot, generation, targets, requestGeneration,
                   ackGeneration, lastActivationSameRoot,
                   lastActivationReloaded, targetSnapshotRoot>>

ReleaseReference(root) ==
    /\ references[root] > 0
    /\ \A cpu \in Cpus: activeRoot[cpu] # root
    /\ references' = [references EXCEPT ![root] = @ - 1]
    /\ UNCHANGED <<activeRoot, eligible, rootState, phase, mutationKind,
                   mutationRoot, generation, targets, requestGeneration,
                   ackGeneration, lastActivationSameRoot,
                   lastActivationReloaded, targetSnapshotRoot>>

Activate(cpu, root) ==
    /\ rootState[root] = RootLive
    /\ references[root] > 0
    /\ lastActivationSameRoot' = (activeRoot[cpu] = root)
    /\ lastActivationReloaded' = (activeRoot[cpu] # root)
    /\ activeRoot' = [activeRoot EXCEPT ![cpu] = root]
    /\ UNCHANGED <<eligible, rootState, references, phase, mutationKind,
                   mutationRoot, generation, targets, requestGeneration,
                   ackGeneration, targetSnapshotRoot>>

Deactivate(cpu) ==
    /\ activeRoot[cpu] # NoneRoot
    /\ activeRoot' = [activeRoot EXCEPT ![cpu] = NoneRoot]
    /\ lastActivationSameRoot' = FALSE
    /\ lastActivationReloaded' = FALSE
    /\ UNCHANGED <<eligible, rootState, references, phase, mutationKind,
                   mutationRoot, generation, targets, requestGeneration,
                   ackGeneration, targetSnapshotRoot>>

Admit(cpu) ==
    /\ phase = Idle
    /\ activeRoot[cpu] \in Roots
    /\ ~eligible[cpu]
    /\ eligible' = [eligible EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<activeRoot, rootState, references, phase, mutationKind,
                   mutationRoot, generation, targets, requestGeneration,
                   ackGeneration, lastActivationSameRoot,
                   lastActivationReloaded, targetSnapshotRoot>>

BeginMutation(root) ==
    /\ phase = Idle
    /\ rootState[root] = RootLive
    /\ phase' = Editing
    /\ mutationKind' = EditMutation
    /\ mutationRoot' = root
    \* Target capture occurs only after the edit at Publish. An activation
    \* before then is observed; a later changed activation reloads CR3 before
    \* publishing the new root.
    /\ targets' = {}
    /\ UNCHANGED <<activeRoot, eligible, rootState, references, generation,
                   requestGeneration, ackGeneration, lastActivationSameRoot,
                   lastActivationReloaded, targetSnapshotRoot>>

BeginGlobalMutation ==
    /\ phase = Idle
    /\ phase' = Editing
    /\ mutationKind' = GlobalMutation
    /\ mutationRoot' = GlobalRoot
    /\ targets' = {}
    /\ UNCHANGED <<activeRoot, eligible, rootState, references, generation,
                   requestGeneration, ackGeneration, lastActivationSameRoot,
                   lastActivationReloaded, targetSnapshotRoot>>

BeginRetirement(root) ==
    /\ phase = Idle
    /\ rootState[root] = RootLive
    /\ references[root] = 0
    /\ \A cpu \in Cpus: activeRoot[cpu] # root
    /\ rootState' = [rootState EXCEPT ![root] = RootRetiring]
    /\ phase' = Editing
    /\ mutationKind' = RetireMutation
    /\ mutationRoot' = root
    /\ targets' = {}
    /\ UNCHANGED <<activeRoot, eligible, references, generation,
                   requestGeneration, ackGeneration, lastActivationSameRoot,
                   lastActivationReloaded, targetSnapshotRoot>>

Publish ==
    /\ phase = Editing
    /\ LET publishTargets ==
              IF mutationKind = EditMutation
              THEN AddressSpaceTargets(mutationRoot)
              ELSE EligibleTargets
       IN /\ targets' = publishTargets
          /\ requestGeneration' =
              [cpu \in Cpus |->
                  IF cpu \in publishTargets
                  THEN generation + 1 ELSE requestGeneration[cpu]]
          /\ ackGeneration' =
              [cpu \in Cpus |->
                  IF cpu \in publishTargets THEN 0 ELSE ackGeneration[cpu]]
          /\ targetSnapshotRoot' = activeRoot
    /\ generation' = generation + 1
    /\ phase' = Published
    /\ UNCHANGED <<activeRoot, eligible, rootState, references, mutationKind,
                   mutationRoot, lastActivationSameRoot,
                   lastActivationReloaded>>

Acknowledge(cpu) ==
    /\ phase = Published
    /\ cpu \in targets
    /\ requestGeneration[cpu] = generation
    /\ ackGeneration' = [ackGeneration EXCEPT ![cpu] = generation]
    /\ UNCHANGED <<activeRoot, eligible, rootState, references, phase,
                   mutationKind, mutationRoot, generation, targets,
                   requestGeneration, lastActivationSameRoot,
                   lastActivationReloaded, targetSnapshotRoot>>

Finish ==
    /\ phase = Published
    /\ \A cpu \in targets: ackGeneration[cpu] = generation
    /\ rootState' =
        IF mutationKind = RetireMutation
        THEN [rootState EXCEPT ![mutationRoot] = RootReclaimed]
        ELSE rootState
    /\ phase' = Complete
    /\ UNCHANGED <<activeRoot, eligible, references, mutationKind,
                   mutationRoot, generation, targets, requestGeneration,
                   ackGeneration, lastActivationSameRoot,
                   lastActivationReloaded, targetSnapshotRoot>>

Terminal ==
    /\ phase = Complete
    /\ UNCHANGED vars

Next ==
    \/ \E root \in Roots: AcquireReference(root)
    \/ \E root \in Roots: ReleaseReference(root)
    \/ \E cpu \in Cpus, root \in Roots: Activate(cpu, root)
    \/ \E cpu \in Cpus: Deactivate(cpu)
    \/ \E cpu \in Cpus: Admit(cpu)
    \/ \E root \in Roots: BeginMutation(root)
    \/ BeginGlobalMutation
    \/ \E root \in Roots: BeginRetirement(root)
    \/ Publish
    \/ \E cpu \in Cpus: Acknowledge(cpu)
    \/ Finish
    \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ activeRoot \in [Cpus -> Roots \cup {NoneRoot}]
    /\ eligible \in [Cpus -> BOOLEAN]
    /\ rootState \in [Roots -> {RootLive, RootRetiring, RootReclaimed}]
    /\ references \in [Roots -> 0..MaxRefs]
    /\ phase \in {Idle, Editing, Published, Complete}
    /\ mutationKind \in
        {NoMutation, EditMutation, GlobalMutation, RetireMutation}
    /\ mutationRoot \in Roots \cup {NoneRoot, GlobalRoot}
    /\ generation \in Nat
    /\ targets \subseteq Cpus
    /\ requestGeneration \in [Cpus -> Nat]
    /\ ackGeneration \in [Cpus -> Nat]
    /\ lastActivationSameRoot \in BOOLEAN
    /\ lastActivationReloaded \in BOOLEAN
    /\ targetSnapshotRoot \in [Cpus -> Roots \cup {NoneRoot}]

SameRootActivationDoesNotReload ==
    lastActivationSameRoot => ~lastActivationReloaded

TargetsMatchMutationScope ==
    phase \in {Published, Complete} =>
        targets = IF mutationKind = EditMutation
                  THEN {cpu \in Cpus:
                            eligible[cpu] /\ targetSnapshotRoot[cpu] = mutationRoot}
                  ELSE EligibleTargets

PublishedTargetsOwnExactGeneration ==
    phase = Published =>
        \A cpu \in targets: requestGeneration[cpu] = generation

FinishRequiresEveryAcknowledgement ==
    phase = Complete =>
        \A cpu \in targets: ackGeneration[cpu] = generation

RetiredRootsOwnNoReference ==
    \A root \in Roots:
        rootState[root] \in {RootRetiring, RootReclaimed} => references[root] = 0

ActiveRootsAreLiveAndReferenced ==
    \A cpu \in Cpus:
        activeRoot[cpu] # NoneRoot =>
            /\ rootState[activeRoot[cpu]] = RootLive
            /\ references[activeRoot[cpu]] > 0

ReclaimedRootsCannotReactivate ==
    \A root \in Roots:
        rootState[root] = RootReclaimed =>
            /\ references[root] = 0
            /\ \A cpu \in Cpus: activeRoot[cpu] # root

=============================================================================
