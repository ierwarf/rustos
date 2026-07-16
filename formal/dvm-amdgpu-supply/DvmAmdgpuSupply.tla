-------------------------- MODULE DvmAmdgpuSupply --------------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
AMD physical display-driver supply contract for the current PCI 1002:1900
Phoenix/HawkPoint target.

Concrete owners:
  driver-domains/linux/board/linux.fragment
  driver-domains/linux/configs/rustos_linux_dvm_x86_64_defconfig
  driver-domains/linux/scripts/verify-module-signatures.sh
  driver-domains/linux/board/overlay/etc/init.d/S48rustos-dvm-net

The kernel-produced PCI modalias is the only selector.  KMS admission requires
the signed upstream amdgpu module, its bound signing certificate, and every
firmware payload consumed by this GC 11.0.1 target.  An incomplete image is a
failed admission, never a reason to start a degraded relay.
***************************************************************************)

RequiredFirmware == {
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
          moduleSigned,
          signingCertificateBound,
          kernelModaliasSelected,
          computeAuthority,
          kmsReady,
          relayReady,
          rejected,
          revoked

vars == <<firmwarePresent, moduleSigned, signingCertificateBound,
          kernelModaliasSelected, computeAuthority, kmsReady, relayReady,
          rejected, revoked>>

Init ==
    /\ firmwarePresent = {}
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
    /\ UNCHANGED <<moduleSigned, signingCertificateBound,
                  kernelModaliasSelected, computeAuthority, kmsReady,
                  relayReady, rejected, revoked>>

BindSignedAmdgpu ==
    /\ ~rejected
    /\ ~revoked
    /\ ~moduleSigned
    /\ moduleSigned' = TRUE
    /\ signingCertificateBound' = TRUE
    /\ UNCHANGED <<firmwarePresent, kernelModaliasSelected,
                  computeAuthority, kmsReady, relayReady, rejected, revoked>>

SelectKernelPciModalias ==
    /\ ~rejected
    /\ ~revoked
    /\ ~kernelModaliasSelected
    /\ kernelModaliasSelected' = TRUE
    /\ UNCHANGED <<firmwarePresent, moduleSigned, signingCertificateBound,
                  computeAuthority, kmsReady, relayReady, rejected, revoked>>

InitializeKms ==
    /\ ~rejected
    /\ ~revoked
    /\ firmwarePresent = RequiredFirmware
    /\ moduleSigned
    /\ signingCertificateBound
    /\ kernelModaliasSelected
    /\ ~computeAuthority
    /\ ~kmsReady
    /\ kmsReady' = TRUE
    /\ UNCHANGED <<firmwarePresent, moduleSigned, signingCertificateBound,
                  kernelModaliasSelected, computeAuthority, relayReady,
                  rejected, revoked>>

RejectIncompleteSupply ==
    /\ ~rejected
    /\ ~kmsReady
    /\ (firmwarePresent # RequiredFirmware
        \/ ~moduleSigned
        \/ ~signingCertificateBound
        \/ ~kernelModaliasSelected)
    /\ rejected' = TRUE
    /\ UNCHANGED <<firmwarePresent, moduleSigned, signingCertificateBound,
                  kernelModaliasSelected, computeAuthority, kmsReady,
                  relayReady, revoked>>

StartRelay ==
    /\ kmsReady
    /\ ~relayReady
    /\ ~rejected
    /\ ~revoked
    /\ relayReady' = TRUE
    /\ UNCHANGED <<firmwarePresent, moduleSigned, signingCertificateBound,
                  kernelModaliasSelected, computeAuthority, kmsReady,
                  rejected, revoked>>

RevokeDisplay ==
    /\ kmsReady \/ relayReady
    /\ kmsReady' = FALSE
    /\ relayReady' = FALSE
    /\ revoked' = TRUE
    /\ UNCHANGED <<firmwarePresent, moduleSigned, signingCertificateBound,
                  kernelModaliasSelected, computeAuthority, rejected>>

Next ==
    \/ \E firmware \in RequiredFirmware: InstallFirmware(firmware)
    \/ BindSignedAmdgpu
    \/ SelectKernelPciModalias
    \/ InitializeKms
    \/ RejectIncompleteSupply
    \/ StartRelay
    \/ RevokeDisplay

TypeOK ==
    /\ firmwarePresent \subseteq RequiredFirmware
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
        /\ moduleSigned
        /\ signingCertificateBound
        /\ kernelModaliasSelected

RelayRequiresCompleteKms == relayReady => kmsReady
DisplayDvmHasNoComputeAuthority == ~computeAuthority
RejectedSupplyNeverStarts == rejected => ~kmsReady /\ ~relayReady
RevokedSupplyStaysOffline == revoked => ~kmsReady /\ ~relayReady

Spec == Init /\ [][Next]_vars

=============================================================================
