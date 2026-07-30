-------------------- MODULE TlbShootdownLifecycle --------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Models serialized address-space activation, page-table mutation, exact target
snapshot, generation mailbox acknowledgement, and reclaim admission.

Concrete owners:
  * kernel/hal/src/arch/tlb_shootdown.rs
  * kernel/mm/src/memory/{address_space,kernel_vm}.rs
***************************************************************************)

CONSTANT Cpus, Roots

NoneRoot == "none"
GlobalRoot == "global"
Idle == "idle"
Editing == "editing"
Published == "published"
Reclaimed == "reclaimed"

VARIABLES activeRoot, eligible, phase, mutationRoot, generation,
          targets, requestGeneration, ackGeneration

vars == <<activeRoot, eligible, phase, mutationRoot, generation,
          targets, requestGeneration, ackGeneration>>

Init ==
    /\ activeRoot = [cpu \in Cpus |-> NoneRoot]
    /\ eligible = [cpu \in Cpus |-> FALSE]
    /\ phase = Idle
    /\ mutationRoot = NoneRoot
    /\ generation = 0
    /\ targets = {}
    /\ requestGeneration = [cpu \in Cpus |-> 0]
    /\ ackGeneration = [cpu \in Cpus |-> 0]

Activate(cpu, root) ==
    /\ phase = Idle
    /\ root \in Roots
    /\ activeRoot' = [activeRoot EXCEPT ![cpu] = root]
    /\ UNCHANGED <<eligible, phase, mutationRoot, generation, targets,
                   requestGeneration, ackGeneration>>

Admit(cpu) ==
    /\ phase = Idle
    /\ activeRoot[cpu] \in Roots
    /\ ~eligible[cpu]
    /\ eligible' = [eligible EXCEPT ![cpu] = TRUE]
    /\ UNCHANGED <<activeRoot, phase, mutationRoot, generation, targets,
                   requestGeneration, ackGeneration>>

Begin(root) ==
    /\ phase = Idle
    /\ root \in Roots \cup {GlobalRoot}
    /\ phase' = Editing
    /\ mutationRoot' = root
    /\ targets' = {
        cpu \in Cpus:
            eligible[cpu]
            /\ (root = GlobalRoot \/ activeRoot[cpu] = root)
       }
    /\ UNCHANGED <<activeRoot, eligible, generation,
                   requestGeneration, ackGeneration>>

Publish ==
    /\ phase = Editing
    /\ generation' = generation + 1
    /\ requestGeneration' =
        [cpu \in Cpus |->
            IF cpu \in targets THEN generation + 1 ELSE requestGeneration[cpu]]
    /\ ackGeneration' =
        [cpu \in Cpus |->
            IF cpu \in targets THEN 0 ELSE ackGeneration[cpu]]
    /\ phase' = Published
    /\ UNCHANGED <<activeRoot, eligible, mutationRoot, targets>>

Acknowledge(cpu) ==
    /\ phase = Published
    /\ cpu \in targets
    /\ requestGeneration[cpu] = generation
    /\ ackGeneration' = [ackGeneration EXCEPT ![cpu] = generation]
    /\ UNCHANGED <<activeRoot, eligible, phase, mutationRoot, generation,
                   targets, requestGeneration>>

Reclaim ==
    /\ phase = Published
    /\ \A cpu \in targets: ackGeneration[cpu] = generation
    /\ phase' = Reclaimed
    /\ UNCHANGED <<activeRoot, eligible, mutationRoot, generation, targets,
                   requestGeneration, ackGeneration>>

Terminal ==
    /\ phase = Reclaimed
    /\ UNCHANGED vars

Next ==
    \/ \E cpu \in Cpus, root \in Roots: Activate(cpu, root)
    \/ \E cpu \in Cpus: Admit(cpu)
    \/ \E root \in Roots \cup {GlobalRoot}: Begin(root)
    \/ Publish
    \/ \E cpu \in Cpus: Acknowledge(cpu)
    \/ Reclaim
    \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ activeRoot \in [Cpus -> Roots \cup {NoneRoot}]
    /\ eligible \in [Cpus -> BOOLEAN]
    /\ phase \in {Idle, Editing, Published, Reclaimed}
    /\ mutationRoot \in Roots \cup {NoneRoot, GlobalRoot}
    /\ generation \in Nat
    /\ targets \subseteq Cpus
    /\ requestGeneration \in [Cpus -> Nat]
    /\ ackGeneration \in [Cpus -> Nat]

TargetsMatchSnapshot ==
    phase \in {Editing, Published} =>
        \A cpu \in targets:
            /\ eligible[cpu]
            /\ (mutationRoot = GlobalRoot
                \/ activeRoot[cpu] = mutationRoot)

PublishedTargetsOwnExactGeneration ==
    phase = Published =>
        \A cpu \in targets: requestGeneration[cpu] = generation

ReclaimRequiresEveryAcknowledgement ==
    phase = Reclaimed =>
        \A cpu \in targets: ackGeneration[cpu] = generation

=============================================================================
