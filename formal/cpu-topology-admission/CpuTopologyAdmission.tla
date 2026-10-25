---------------------- MODULE CpuTopologyAdmission ----------------------
EXTENDS Naturals

(***************************************************************************
Models atomic MADT processor admission. Firmware is untrusted input: malformed
entry lengths, duplicate identities, an absent BSP, an invalid local APIC
address, or zero enabled CPUs must publish no partial topology. Disabled and
hot-add-only entries are not candidates. Enabled CPUs beyond MaxCpus are left
unstarted: the topology publishes the first MaxCpus of them. An admitted
topology normalizes the executing BSP to logical CPU zero before publication.

Concrete owner:
  * kernel/hal/src/arch/acpi.rs
***************************************************************************)

CONSTANT MaxCpus

Min(a, b) == IF a < b THEN a ELSE b

VARIABLES candidateCount, lengthsValid, identitiesUnique,
          apicAddressValid, bspPresent, phase, publishedCount, bspLogicalZero

vars == <<candidateCount, lengthsValid, identitiesUnique,
          apicAddressValid, bspPresent, phase, publishedCount, bspLogicalZero>>

Init ==
    /\ candidateCount \in 0..(MaxCpus + 1)
    /\ lengthsValid \in BOOLEAN
    /\ identitiesUnique \in BOOLEAN
    /\ apicAddressValid \in BOOLEAN
    /\ bspPresent \in BOOLEAN
    /\ phase = "inspect"
    /\ publishedCount = 0
    /\ bspLogicalZero = FALSE

Inspect ==
    /\ phase = "inspect"
    /\ phase' = "decide"
    /\ UNCHANGED <<candidateCount, lengthsValid, identitiesUnique,
                  apicAddressValid, bspPresent,
                  publishedCount, bspLogicalZero>>

Admit ==
    /\ phase = "decide"
    /\ candidateCount \in 1..(MaxCpus + 1)
    /\ lengthsValid
    /\ identitiesUnique
    /\ apicAddressValid
    /\ bspPresent
    /\ phase' = "published"
    /\ publishedCount' = Min(candidateCount, MaxCpus)
    /\ bspLogicalZero' = TRUE
    /\ UNCHANGED <<candidateCount, lengthsValid, identitiesUnique,
                  apicAddressValid, bspPresent>>

Reject ==
    /\ phase = "decide"
    /\ ~(candidateCount \in 1..(MaxCpus + 1)
         /\ lengthsValid
         /\ identitiesUnique
              /\ apicAddressValid
         /\ bspPresent)
    /\ phase' = "rejected"
    /\ publishedCount' = 0
    /\ bspLogicalZero' = FALSE
    /\ UNCHANGED <<candidateCount, lengthsValid, identitiesUnique,
                  apicAddressValid, bspPresent>>

Terminal ==
    /\ phase \in {"published", "rejected"}
    /\ UNCHANGED vars

Next == Inspect \/ Admit \/ Reject \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ candidateCount \in 0..(MaxCpus + 1)
    /\ lengthsValid \in BOOLEAN
    /\ identitiesUnique \in BOOLEAN
    /\ apicAddressValid \in BOOLEAN
    /\ bspPresent \in BOOLEAN
    /\ phase \in {"inspect", "decide", "published", "rejected"}
    /\ publishedCount \in 0..MaxCpus
    /\ bspLogicalZero \in BOOLEAN

PublicationIsAtomic ==
    publishedCount = 0 \/ publishedCount = Min(candidateCount, MaxCpus)

PublishedTopologyIsAdmitted ==
    phase = "published" =>
        /\ candidateCount \in 1..(MaxCpus + 1)
        /\ lengthsValid
        /\ identitiesUnique
            /\ apicAddressValid
        /\ bspPresent
        /\ publishedCount = Min(candidateCount, MaxCpus)
        /\ bspLogicalZero

RejectedTopologyPublishesNothing ==
    phase = "rejected" => publishedCount = 0 /\ ~bspLogicalZero

PublishedBspOwnsLogicalZero ==
    phase = "published" => bspPresent /\ bspLogicalZero

=============================================================================
