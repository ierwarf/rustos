-------------------------- MODULE BootVolumeAdmission --------------------------
EXTENDS FiniteSets

(*******************************************************************************
Models the physical boot-volume admission boundary before rootd exists.

Concrete owners and source anchors:
  * exact BootInfo identity validation:
      kernel/io-manager/src/storage/block/boot.rs
      libs/storage-core/src/lib.rs (BootVolumeLocator)
  * Multiboot2 extent-manifest admission when identity is unavailable:
      kernel/io-manager/src/storage/boot_volume.rs
      kernel/nucleus-core/src/multiboot2.rs

Multiboot2 supplies an immutable root-extent manifest but no physical volume
identity. Ring0 may therefore select a volume only if it is the sole FAT
candidate. A supplied identity has stronger authority: an exact match may be
selected, while a mismatch must never degrade into discovery. This is a
bootstrap substrate contract, not filesystem namespace policy.
*******************************************************************************)

CONSTANTS CandidateDevices, ExpectedDevice

IdentityStates == {"absent", "exact", "mismatch"}
Origins == {"none", "exact-identity", "manifest-discovery"}
Outcomes == {"pending", "selected", "denied"}
NoDevice == "no-device"

VARIABLES identity, manifestPresent, candidates, outcome, selected, origin

vars == <<identity, manifestPresent, candidates, outcome, selected, origin>>

Init ==
    /\ identity \in IdentityStates
    /\ manifestPresent \in BOOLEAN
    /\ candidates \in SUBSET CandidateDevices
    /\ outcome = "pending"
    /\ selected = NoDevice
    /\ origin = "none"

ExactIdentitySelect ==
    /\ outcome = "pending"
    /\ identity = "exact"
    /\ ExpectedDevice \in candidates
    /\ outcome' = "selected"
    /\ selected' = ExpectedDevice
    /\ origin' = "exact-identity"
    /\ UNCHANGED <<identity, manifestPresent, candidates>>

ManifestDiscoverySelect ==
    /\ outcome = "pending"
    /\ identity = "absent"
    /\ manifestPresent
    /\ Cardinality(candidates) = 1
    /\ outcome' = "selected"
    /\ selected' = CHOOSE device \in candidates: TRUE
    /\ origin' = "manifest-discovery"
    /\ UNCHANGED <<identity, manifestPresent, candidates>>

Deny ==
    /\ outcome = "pending"
    /\ ~((identity = "exact" /\ ExpectedDevice \in candidates)
       \/ (identity = "absent" /\ manifestPresent /\ Cardinality(candidates) = 1))
    /\ outcome' = "denied"
    /\ selected' = NoDevice
    /\ origin' = "none"
    /\ UNCHANGED <<identity, manifestPresent, candidates>>

Next == ExactIdentitySelect \/ ManifestDiscoverySelect \/ Deny

TypeOK ==
    /\ identity \in IdentityStates
    /\ manifestPresent \in BOOLEAN
    /\ candidates \in SUBSET CandidateDevices
    /\ outcome \in Outcomes
    /\ selected \in CandidateDevices \cup {NoDevice}
    /\ origin \in Origins

SelectedDeviceIsKnown ==
    selected # NoDevice => selected \in candidates

ExactIdentitySelectsOnlyItsTarget ==
    origin = "exact-identity" =>
        /\ identity = "exact"
        /\ selected = ExpectedDevice

ManifestDiscoveryIsAuthorizedAndUnique ==
    origin = "manifest-discovery" =>
        /\ identity = "absent"
        /\ manifestPresent
        /\ candidates = {selected}

SuppliedIdentityNeverDegradesToDiscovery ==
    identity \in {"exact", "mismatch"} => origin # "manifest-discovery"

MismatchedIdentityFailsClosed ==
    identity = "mismatch" =>
        /\ selected = NoDevice
        /\ origin = "none"

IdentityAbsenceRequiresManifest ==
    identity = "absent" /\ selected # NoDevice => manifestPresent

SelectedOutcomeIsTerminal ==
    (outcome = "selected") = (selected # NoDevice)

Spec == Init /\ [][Next]_vars
================================================================================
