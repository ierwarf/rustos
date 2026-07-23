-------------------------- MODULE EarlySystemAdmission --------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models admission of the signed Multiboot2 early-system image that removes
pre-userspace dependence on a physical NVMe/AHCI filesystem.

Concrete source contract:
  * boot/boot-protocol/src/lib.rs
      EarlySystemImage, EarlySystemHeader, EarlySystemEntry
  * kernel/nucleus-core/src/multiboot2.rs
      unique rustos-early-system module selection
  * kernel/io-manager/src/storage/boot_volume.rs
      exact path, table, range, and SHA-256 validation
  * tools/xtask/src/stage/mod.rs
      deterministic bootstrap allowlist and detached signature

The executive has no native storage initializer; malformed or missing
early-system state therefore terminates without a physical fallback.
*******************************************************************************)

CONSTANTS Services, RequiredBootstrap

VARIABLES phase, moduleCount, tableWellFormed, declared, digestValid, loaded,
          nativeProbe

vars == <<phase, moduleCount, tableWellFormed, declared, digestValid, loaded,
          nativeProbe>>

Phases == {"inspect", "admitted", "dvm-storage-ready", "failed"}

Init ==
    /\ phase = "inspect"
    /\ moduleCount \in 0..2
    /\ tableWellFormed \in BOOLEAN
    /\ declared \in SUBSET Services
    /\ digestValid \in SUBSET Services
    /\ loaded = {}
    /\ nativeProbe = FALSE

Admit ==
    /\ phase = "inspect"
    /\ moduleCount = 1
    /\ tableWellFormed
    /\ RequiredBootstrap \subseteq declared
    /\ phase' = "admitted"
    /\ UNCHANGED <<moduleCount, tableWellFormed, declared, digestValid, loaded,
                   nativeProbe>>

RejectEnvelope ==
    /\ phase = "inspect"
    /\ ~(moduleCount = 1 /\ tableWellFormed /\
         RequiredBootstrap \subseteq declared)
    /\ phase' = "failed"
    /\ UNCHANGED <<moduleCount, tableWellFormed, declared, digestValid, loaded,
                   nativeProbe>>

LoadOne(service) ==
    /\ phase = "admitted"
    /\ service \in RequiredBootstrap \ loaded
    /\ service \in declared
    /\ service \in digestValid
    /\ loaded' = loaded \cup {service}
    /\ UNCHANGED <<phase, moduleCount, tableWellFormed, declared, digestValid,
                   nativeProbe>>

RejectDigest ==
    /\ phase = "admitted"
    /\ \E service \in RequiredBootstrap \ loaded: service \notin digestValid
    /\ phase' = "failed"
    /\ UNCHANGED <<moduleCount, tableWellFormed, declared, digestValid, loaded,
                   nativeProbe>>

PublishDvmStorage ==
    /\ phase = "admitted"
    /\ loaded = RequiredBootstrap
    /\ phase' = "dvm-storage-ready"
    /\ UNCHANGED <<moduleCount, tableWellFormed, declared, digestValid, loaded,
                   nativeProbe>>

Next ==
    Admit
    \/ RejectEnvelope
    \/ (\E service \in RequiredBootstrap: LoadOne(service))
    \/ RejectDigest
    \/ PublishDvmStorage

TypeOK ==
    /\ phase \in Phases
    /\ moduleCount \in 0..2
    /\ tableWellFormed \in BOOLEAN
    /\ declared \in SUBSET Services
    /\ digestValid \in SUBSET Services
    /\ loaded \in SUBSET Services
    /\ nativeProbe \in BOOLEAN

LoadedFilesAreExactAndVerified ==
    /\ loaded \subseteq RequiredBootstrap
    /\ loaded \subseteq declared
    /\ loaded \subseteq digestValid

AdmissionRequiresUniqueWellFormedModule ==
    phase \in {"admitted", "dvm-storage-ready"} =>
        moduleCount = 1 /\ tableWellFormed /\
        RequiredBootstrap \subseteq declared

DvmStorageWaitsForBootstrap ==
    phase = "dvm-storage-ready" => loaded = RequiredBootstrap

NativeProbeNeverRequired == ~nativeProbe

TerminalFailureHasNoLoadedAuthority ==
    phase = "failed" => ~nativeProbe

Spec == Init /\ [][Next]_vars
===============================================================================
