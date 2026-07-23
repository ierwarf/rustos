---------------------- MODULE AcpiTableAdmission ----------------------
EXTENDS Naturals

(***************************************************************************
Owner: kernel HAL ACPI/MCFG/HPET admission.
Firmware bytes are external input. Root tables require bounded length,
checksum, exact RSDT/XSDT signature and entry alignment. MCFG publication is
atomic: every ECAM region must be aligned, in the mapped physical aperture,
bounded in count, and non-overlapping. A rejected table publishes no partial
ECAM authority and may only select the bounded legacy PCI fallback. HPET is
published only after exact GAS shape, access size, alignment and range checks.
***************************************************************************)

CONSTANT MaxRegions

VARIABLES phase, regionCount, mcfgDecided, hpetDecided, hpetReady, legacyPci
vars == <<phase, regionCount, mcfgDecided, hpetDecided, hpetReady, legacyPci>>

Init ==
    /\ phase = "start"
    /\ regionCount = 0
    /\ mcfgDecided = FALSE
    /\ hpetDecided = FALSE
    /\ hpetReady = FALSE
    /\ legacyPci = FALSE

AdmitRoot ==
    /\ phase = "start"
    /\ phase' = "root-valid"
    /\ UNCHANGED <<regionCount, mcfgDecided, hpetDecided, hpetReady, legacyPci>>

RejectRoot ==
    /\ phase = "start"
    /\ phase' = "done"
    /\ mcfgDecided' = TRUE
    /\ hpetDecided' = TRUE
    /\ legacyPci' = TRUE
    /\ UNCHANGED <<regionCount, hpetReady>>

AdmitMcfg(count) ==
    /\ phase = "root-valid"
    /\ ~mcfgDecided
    /\ count \in 1..MaxRegions
    /\ regionCount' = count
    /\ mcfgDecided' = TRUE
    /\ UNCHANGED <<phase, hpetDecided, hpetReady, legacyPci>>

RejectMcfg ==
    /\ phase = "root-valid"
    /\ ~mcfgDecided
    /\ regionCount = 0
    /\ mcfgDecided' = TRUE
    /\ legacyPci' = TRUE
    /\ UNCHANGED <<phase, regionCount, hpetDecided, hpetReady>>

AdmitHpet ==
    /\ phase = "root-valid"
    /\ ~hpetDecided
    /\ hpetDecided' = TRUE
    /\ hpetReady' = TRUE
    /\ UNCHANGED <<phase, regionCount, mcfgDecided, legacyPci>>

RejectHpet ==
    /\ phase = "root-valid"
    /\ ~hpetDecided
    /\ hpetDecided' = TRUE
    /\ UNCHANGED <<phase, regionCount, mcfgDecided, hpetReady, legacyPci>>

Finish ==
    /\ phase = "root-valid"
    /\ mcfgDecided /\ hpetDecided
    /\ phase' = "done"
    /\ UNCHANGED <<regionCount, mcfgDecided, hpetDecided, hpetReady, legacyPci>>

Next ==
    \/ AdmitRoot
    \/ RejectRoot
    \/ RejectMcfg
    \/ AdmitHpet
    \/ RejectHpet
    \/ Finish
    \/ \E count \in 1..MaxRegions: AdmitMcfg(count)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {"start", "root-valid", "done"}
    /\ regionCount \in 0..MaxRegions
    /\ mcfgDecided \in BOOLEAN
    /\ hpetDecided \in BOOLEAN
    /\ hpetReady \in BOOLEAN
    /\ legacyPci \in BOOLEAN

EcamRequiresAtomicAdmission ==
    regionCount > 0 => mcfgDecided /\ ~legacyPci

LegacyPublishesNoEcam ==
    legacyPci => regionCount = 0

HpetRequiresExactAdmission ==
    hpetReady => hpetDecided /\ phase # "start"

DoneHasBoundedDecisions ==
    phase = "done" => mcfgDecided /\ hpetDecided

=============================================================================
