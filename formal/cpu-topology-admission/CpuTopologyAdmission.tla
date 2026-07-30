---------------------- MODULE CpuTopologyAdmission ----------------------
EXTENDS Naturals

(***************************************************************************
Models atomic MADT processor admission. Firmware is untrusted input: malformed
entry lengths, duplicate identities, hot-add-only processors, an absent BSP,
an invalid local APIC address, zero CPUs, or capacity overflow must publish no
partial topology. An admitted topology normalizes the executing BSP to logical
CPU zero before publication.

Concrete owner:
  * kernel/hal/src/arch/acpi.rs
***************************************************************************)

CONSTANT MaxCpus

VARIABLES candidateCount, lengthsValid, identitiesUnique, fixedCpuEnvelope,
          apicAddressValid, bspPresent, phase, publishedCount, bspLogicalZero

vars == <<candidateCount, lengthsValid, identitiesUnique, fixedCpuEnvelope,
          apicAddressValid, bspPresent, phase, publishedCount, bspLogicalZero>>

Init ==
    /\ candidateCount \in 0..(MaxCpus + 1)
    /\ lengthsValid \in BOOLEAN
    /\ identitiesUnique \in BOOLEAN
    /\ fixedCpuEnvelope \in BOOLEAN
    /\ apicAddressValid \in BOOLEAN
    /\ bspPresent \in BOOLEAN
    /\ phase = "inspect"
    /\ publishedCount = 0
    /\ bspLogicalZero = FALSE

Inspect ==
    /\ phase = "inspect"
    /\ phase' = "decide"
    /\ UNCHANGED <<candidateCount, lengthsValid, identitiesUnique,
                  fixedCpuEnvelope, apicAddressValid, bspPresent,
                  publishedCount, bspLogicalZero>>

Admit ==
    /\ phase = "decide"
    /\ candidateCount \in 1..MaxCpus
    /\ lengthsValid
    /\ identitiesUnique
    /\ fixedCpuEnvelope
    /\ apicAddressValid
    /\ bspPresent
    /\ phase' = "published"
    /\ publishedCount' = candidateCount
    /\ bspLogicalZero' = TRUE
    /\ UNCHANGED <<candidateCount, lengthsValid, identitiesUnique,
                  fixedCpuEnvelope, apicAddressValid, bspPresent>>

Reject ==
    /\ phase = "decide"
    /\ ~(candidateCount \in 1..MaxCpus
         /\ lengthsValid
         /\ identitiesUnique
         /\ fixedCpuEnvelope
         /\ apicAddressValid
         /\ bspPresent)
    /\ phase' = "rejected"
    /\ publishedCount' = 0
    /\ bspLogicalZero' = FALSE
    /\ UNCHANGED <<candidateCount, lengthsValid, identitiesUnique,
                  fixedCpuEnvelope, apicAddressValid, bspPresent>>

Terminal ==
    /\ phase \in {"published", "rejected"}
    /\ UNCHANGED vars

Next == Inspect \/ Admit \/ Reject \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ candidateCount \in 0..(MaxCpus + 1)
    /\ lengthsValid \in BOOLEAN
    /\ identitiesUnique \in BOOLEAN
    /\ fixedCpuEnvelope \in BOOLEAN
    /\ apicAddressValid \in BOOLEAN
    /\ bspPresent \in BOOLEAN
    /\ phase \in {"inspect", "decide", "published", "rejected"}
    /\ publishedCount \in 0..MaxCpus
    /\ bspLogicalZero \in BOOLEAN

PublicationIsAtomic ==
    publishedCount = 0 \/ publishedCount = candidateCount

PublishedTopologyIsAdmitted ==
    phase = "published" =>
        /\ candidateCount \in 1..MaxCpus
        /\ lengthsValid
        /\ identitiesUnique
        /\ fixedCpuEnvelope
        /\ apicAddressValid
        /\ bspPresent
        /\ publishedCount = candidateCount
        /\ bspLogicalZero

RejectedTopologyPublishesNothing ==
    phase = "rejected" => publishedCount = 0 /\ ~bspLogicalZero

PublishedBspOwnsLogicalZero ==
    phase = "published" => bspPresent /\ bspLogicalZero

=============================================================================
