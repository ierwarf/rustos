-------------------------- MODULE DvmAmdgpuSupply --------------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
AMD physical display-driver supply contract for the current PCI 1002:1900
Phoenix/HawkPoint target.

Concrete owners:
  tools/hostd/src/runtime.rs
  driver-domains/linux/board/linux.fragment
  driver-domains/linux/configs/rustos_linux_dvm_x86_64_defconfig
  driver-domains/linux/scripts/verify-module-signatures.sh
  driver-domains/linux/board/overlay/etc/init.d/S48rustos-dvm-net

The host first selects an exact PCI-identity-matched APU VBIOS from the
checksummed ACPI VFCT table, accepts only an exact populated subsystem pair or
the firmware-defined all-zero absent pair, and validates its 0x55aa and ATOM
header.  It then relocates only the image BDF to the fixed guest slot,
recomputes the table checksum without changing the VBIOS payload, snapshots
the complete table into the owner-private launch directory, and supplies that
exact ACPI table to QEMU with the VFIO function at the same slot.  The
kernel-produced PCI modalias is the only module
selector.  KMS admission requires that VBIOS chain, the signed upstream
amdgpu module, its bound signing certificate, and every firmware payload
consumed by this GC 11.0.1 target.  An incomplete image is a failed admission,
never a reason to start a degraded relay.
***************************************************************************)

RequiredFirmware == {
    "dcn_3_1_4_dmcub.bin",
    "gc_11_0_1_imu.bin",
    "gc_11_0_1_me.bin",
    "gc_11_0_1_mec.bin",
    "gc_11_0_1_mes.bin",
    "gc_11_0_1_mes1.bin",
    "gc_11_0_1_mes_2.bin",
    "gc_11_0_1_pfp.bin",
    "gc_11_0_1_rlc.bin",
    "psp_13_0_4_ta.bin",
    "psp_13_0_4_toc.bin",
    "sdma_6_0_1.bin",
    "vcn_4_0_2.bin"
}

VARIABLES firmwarePresent,
          vfctIdentityMatched,
          vbiosValidated,
          guestBdfRelocated,
          relocatedChecksumValid,
          vbiosSnapshotted,
          vbiosSupplied,
          moduleSigned,
          signingCertificateBound,
          kernelModaliasSelected,
          computeAuthority,
          kmsReady,
          relayReady,
          rejected,
          revoked

vars == <<firmwarePresent, vfctIdentityMatched, vbiosValidated,
          guestBdfRelocated, relocatedChecksumValid, vbiosSnapshotted,
          vbiosSupplied, moduleSigned, signingCertificateBound,
          kernelModaliasSelected, computeAuthority, kmsReady, relayReady,
          rejected, revoked>>

Init ==
    /\ firmwarePresent = {}
    /\ vfctIdentityMatched = FALSE
    /\ vbiosValidated = FALSE
    /\ guestBdfRelocated = FALSE
    /\ relocatedChecksumValid = FALSE
    /\ vbiosSnapshotted = FALSE
    /\ vbiosSupplied = FALSE
    /\ moduleSigned = FALSE
    /\ signingCertificateBound = FALSE
    /\ kernelModaliasSelected = FALSE
    /\ computeAuthority = FALSE
    /\ kmsReady = FALSE
    /\ relayReady = FALSE
    /\ rejected = FALSE
    /\ revoked = FALSE

InstallFirmware(firmware) ==
    /\ ~rejected
    /\ ~revoked
    /\ firmware \in RequiredFirmware \ firmwarePresent
    /\ firmwarePresent' = firmwarePresent \union {firmware}
    /\ UNCHANGED <<vfctIdentityMatched, vbiosValidated, guestBdfRelocated,
                  relocatedChecksumValid, vbiosSnapshotted, vbiosSupplied,
                  moduleSigned, signingCertificateBound,
                  kernelModaliasSelected, computeAuthority, kmsReady,
                  relayReady, rejected, revoked>>

MatchExactVfctIdentity ==
    /\ ~rejected
    /\ ~revoked
    /\ ~vfctIdentityMatched
    /\ vfctIdentityMatched' = TRUE
    /\ UNCHANGED <<firmwarePresent, vbiosValidated, guestBdfRelocated,
                  relocatedChecksumValid, vbiosSnapshotted, vbiosSupplied,
                  moduleSigned, signingCertificateBound,
                  kernelModaliasSelected, computeAuthority, kmsReady,
                  relayReady, rejected, revoked>>

ValidateAtomVbios ==
    /\ ~rejected
    /\ ~revoked
    /\ vfctIdentityMatched
    /\ ~vbiosValidated
    /\ vbiosValidated' = TRUE
    /\ UNCHANGED <<firmwarePresent, vfctIdentityMatched, guestBdfRelocated,
                  relocatedChecksumValid, vbiosSnapshotted, vbiosSupplied,
                  moduleSigned, signingCertificateBound,
                  kernelModaliasSelected, computeAuthority, kmsReady,
                  relayReady, rejected, revoked>>

RelocateVfctGuestBdf ==
    /\ ~rejected
    /\ ~revoked
    /\ vfctIdentityMatched
    /\ vbiosValidated
    /\ ~guestBdfRelocated
    /\ guestBdfRelocated' = TRUE
    /\ UNCHANGED <<firmwarePresent, vfctIdentityMatched, vbiosValidated,
                  relocatedChecksumValid, vbiosSnapshotted, vbiosSupplied,
                  moduleSigned, signingCertificateBound, kernelModaliasSelected,
                  computeAuthority, kmsReady, relayReady, rejected, revoked>>

RecomputeRelocatedVfctChecksum ==
    /\ ~rejected
    /\ ~revoked
    /\ vfctIdentityMatched
    /\ vbiosValidated
    /\ guestBdfRelocated
    /\ ~relocatedChecksumValid
    /\ relocatedChecksumValid' = TRUE
    /\ UNCHANGED <<firmwarePresent, vfctIdentityMatched, vbiosValidated,
                  guestBdfRelocated, vbiosSnapshotted, vbiosSupplied,
                  moduleSigned, signingCertificateBound, kernelModaliasSelected,
                  computeAuthority, kmsReady, relayReady, rejected, revoked>>

SnapshotPrivateVbios ==
    /\ ~rejected
    /\ ~revoked
    /\ vfctIdentityMatched
    /\ vbiosValidated
    /\ guestBdfRelocated
    /\ relocatedChecksumValid
    /\ ~vbiosSnapshotted
    /\ vbiosSnapshotted' = TRUE
    /\ UNCHANGED <<firmwarePresent, vfctIdentityMatched, vbiosValidated,
                  guestBdfRelocated, relocatedChecksumValid, vbiosSupplied,
                  moduleSigned, signingCertificateBound,
                  kernelModaliasSelected, computeAuthority, kmsReady,
                  relayReady, rejected, revoked>>

SupplyVbiosToQemu ==
    /\ ~rejected
    /\ ~revoked
    /\ vfctIdentityMatched
    /\ vbiosValidated
    /\ guestBdfRelocated
    /\ relocatedChecksumValid
    /\ vbiosSnapshotted
    /\ ~vbiosSupplied
    /\ vbiosSupplied' = TRUE
    /\ UNCHANGED <<firmwarePresent, vfctIdentityMatched, vbiosValidated,
                  guestBdfRelocated, relocatedChecksumValid, vbiosSnapshotted,
                  moduleSigned, signingCertificateBound,
                  kernelModaliasSelected, computeAuthority, kmsReady,
                  relayReady, rejected, revoked>>

BindSignedAmdgpu ==
    /\ ~rejected
    /\ ~revoked
    /\ ~moduleSigned
    /\ moduleSigned' = TRUE
    /\ signingCertificateBound' = TRUE
    /\ UNCHANGED <<firmwarePresent, vfctIdentityMatched, vbiosValidated,
                  guestBdfRelocated, relocatedChecksumValid, vbiosSnapshotted,
                  vbiosSupplied, kernelModaliasSelected,
                  computeAuthority, kmsReady, relayReady, rejected, revoked>>

SelectKernelPciModalias ==
    /\ ~rejected
    /\ ~revoked
    /\ ~kernelModaliasSelected
    /\ kernelModaliasSelected' = TRUE
    /\ UNCHANGED <<firmwarePresent, vfctIdentityMatched, vbiosValidated,
                  guestBdfRelocated, relocatedChecksumValid, vbiosSnapshotted,
                  vbiosSupplied, moduleSigned, signingCertificateBound,
                  computeAuthority, kmsReady, relayReady, rejected, revoked>>

InitializeKms ==
    /\ ~rejected
    /\ ~revoked
    /\ firmwarePresent = RequiredFirmware
    /\ vfctIdentityMatched
    /\ vbiosValidated
    /\ guestBdfRelocated
    /\ relocatedChecksumValid
    /\ vbiosSnapshotted
    /\ vbiosSupplied
    /\ moduleSigned
    /\ signingCertificateBound
    /\ kernelModaliasSelected
    /\ ~computeAuthority
    /\ ~kmsReady
    /\ kmsReady' = TRUE
    /\ UNCHANGED <<firmwarePresent, vfctIdentityMatched, vbiosValidated,
                  guestBdfRelocated, relocatedChecksumValid, vbiosSnapshotted,
                  vbiosSupplied, moduleSigned, signingCertificateBound,
                  kernelModaliasSelected, computeAuthority, relayReady,
                  rejected, revoked>>

RejectIncompleteSupply ==
    /\ ~rejected
    /\ ~kmsReady
    /\ (firmwarePresent # RequiredFirmware
        \/ ~vfctIdentityMatched
        \/ ~vbiosValidated
        \/ ~guestBdfRelocated
        \/ ~relocatedChecksumValid
        \/ ~vbiosSnapshotted
        \/ ~vbiosSupplied
        \/ ~moduleSigned
        \/ ~signingCertificateBound
        \/ ~kernelModaliasSelected)
    /\ rejected' = TRUE
    /\ UNCHANGED <<firmwarePresent, vfctIdentityMatched, vbiosValidated,
                  guestBdfRelocated, relocatedChecksumValid, vbiosSnapshotted,
                  vbiosSupplied, moduleSigned, signingCertificateBound,
                  kernelModaliasSelected, computeAuthority, kmsReady,
                  relayReady, revoked>>

StartRelay ==
    /\ kmsReady
    /\ ~relayReady
    /\ ~rejected
    /\ ~revoked
    /\ relayReady' = TRUE
    /\ UNCHANGED <<firmwarePresent, vfctIdentityMatched, vbiosValidated,
                  guestBdfRelocated, relocatedChecksumValid, vbiosSnapshotted,
                  vbiosSupplied, moduleSigned, signingCertificateBound,
                  kernelModaliasSelected, computeAuthority, kmsReady,
                  rejected, revoked>>

RevokeDisplay ==
    /\ kmsReady \/ relayReady
    /\ kmsReady' = FALSE
    /\ relayReady' = FALSE
    /\ vbiosSupplied' = FALSE
    /\ revoked' = TRUE
    /\ UNCHANGED <<firmwarePresent, vfctIdentityMatched, vbiosValidated,
                  guestBdfRelocated, relocatedChecksumValid, vbiosSnapshotted,
                  moduleSigned, signingCertificateBound,
                  kernelModaliasSelected, computeAuthority, rejected>>

Next ==
    \/ \E firmware \in RequiredFirmware: InstallFirmware(firmware)
    \/ MatchExactVfctIdentity
    \/ ValidateAtomVbios
    \/ RelocateVfctGuestBdf
    \/ RecomputeRelocatedVfctChecksum
    \/ SnapshotPrivateVbios
    \/ SupplyVbiosToQemu
    \/ BindSignedAmdgpu
    \/ SelectKernelPciModalias
    \/ InitializeKms
    \/ RejectIncompleteSupply
    \/ StartRelay
    \/ RevokeDisplay

TypeOK ==
    /\ firmwarePresent \subseteq RequiredFirmware
    /\ vfctIdentityMatched \in BOOLEAN
    /\ vbiosValidated \in BOOLEAN
    /\ guestBdfRelocated \in BOOLEAN
    /\ relocatedChecksumValid \in BOOLEAN
    /\ vbiosSnapshotted \in BOOLEAN
    /\ vbiosSupplied \in BOOLEAN
    /\ moduleSigned \in BOOLEAN
    /\ signingCertificateBound \in BOOLEAN
    /\ kernelModaliasSelected \in BOOLEAN
    /\ computeAuthority \in BOOLEAN
    /\ kmsReady \in BOOLEAN
    /\ relayReady \in BOOLEAN
    /\ rejected \in BOOLEAN
    /\ revoked \in BOOLEAN

KmsRequiresCompleteAmdSupply ==
    kmsReady =>
        /\ firmwarePresent = RequiredFirmware
        /\ vfctIdentityMatched
        /\ vbiosValidated
        /\ guestBdfRelocated
        /\ relocatedChecksumValid
        /\ vbiosSnapshotted
        /\ vbiosSupplied
        /\ moduleSigned
        /\ signingCertificateBound
        /\ kernelModaliasSelected

RelayRequiresCompleteKms == relayReady => kmsReady
VbiosValidationRequiresExactIdentity == vbiosValidated => vfctIdentityMatched
GuestBdfRelocationRequiresValidatedImage ==
    guestBdfRelocated => vfctIdentityMatched /\ vbiosValidated
RelocatedChecksumRequiresGuestBdf ==
    relocatedChecksumValid => guestBdfRelocated
VbiosSnapshotRequiresValidatedImage ==
    vbiosSnapshotted =>
        vfctIdentityMatched /\ vbiosValidated /\ guestBdfRelocated
        /\ relocatedChecksumValid
VbiosSupplyRequiresPrivateSnapshot ==
    vbiosSupplied =>
        vfctIdentityMatched /\ vbiosValidated /\ guestBdfRelocated
        /\ relocatedChecksumValid /\ vbiosSnapshotted
DisplayDvmHasNoComputeAuthority == ~computeAuthority
RejectedSupplyNeverStarts == rejected => ~kmsReady /\ ~relayReady
RevokedSupplyStaysOffline == revoked => ~kmsReady /\ ~relayReady

Spec == Init /\ [][Next]_vars

=============================================================================
