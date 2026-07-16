--------------------- MODULE DvmDisplayDriverSupply ---------------------
EXTENDS Naturals

(***************************************************************************
Pinned physical display-driver supply and admission contract.

Concrete owners:
  driver-domains/linux/sources.lock
  driver-domains/linux/package/rustos-dvm-nvidia-open
  driver-domains/linux/board/overlay/etc/init.d/S48rustos-dvm-net
  driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c

The current Blackwell product target admits only one exact NVIDIA open-module
and GSP release.  A kernel-produced PCI modalias selects the module; host
control text cannot select it.  Compute/UVM authority is absent.  Relay
readiness follows complete KMS initialization under kernel-enforced module
signatures whose certificate is bound by the artifact manifest.  Distribution
of the GSP firmware is a separate licensed transition rather than an
implication of a successful local build.
***************************************************************************)

None == "none"
Open == "open"
Pinned == "580.173.02"
KernelModalias == "kernel-pci-modalias"

VARIABLES moduleFlavor,
          moduleRelease,
          firmwareRelease,
          selector,
          computeAuthority,
          moduleSignatureEnforced,
          signingCertificateBound,
          kmsReady,
          relayReady,
          redistributionAuthorized,
          distributed,
          rejected

vars == <<moduleFlavor, moduleRelease, firmwareRelease, selector,
          computeAuthority, moduleSignatureEnforced, signingCertificateBound,
          kmsReady, relayReady,
          redistributionAuthorized, distributed, rejected>>

Init ==
    /\ moduleFlavor = None
    /\ moduleRelease = None
    /\ firmwareRelease = None
    /\ selector = None
    /\ computeAuthority = FALSE
    /\ moduleSignatureEnforced = FALSE
    /\ signingCertificateBound = FALSE
    /\ kmsReady = FALSE
    /\ relayReady = FALSE
    /\ redistributionAuthorized = FALSE
    /\ distributed = FALSE
    /\ rejected = FALSE

StagePinnedOpenStack ==
    /\ moduleFlavor = None
    /\ moduleFlavor' = Open
    /\ moduleRelease' = Pinned
    /\ firmwareRelease' = Pinned
    /\ UNCHANGED <<selector, computeAuthority, moduleSignatureEnforced,
                   signingCertificateBound, kmsReady, relayReady,
                   redistributionAuthorized, distributed, rejected>>

RejectMixedOrProprietaryStack ==
    /\ moduleFlavor = None
    /\ ~rejected
    /\ rejected' = TRUE
    /\ UNCHANGED <<moduleFlavor, moduleRelease, firmwareRelease, selector,
                   computeAuthority, moduleSignatureEnforced,
                   signingCertificateBound, kmsReady, relayReady,
                   redistributionAuthorized, distributed>>

SelectAssignedPciModalias ==
    /\ moduleFlavor = Open
    /\ selector = None
    /\ selector' = KernelModalias
    /\ UNCHANGED <<moduleFlavor, moduleRelease, firmwareRelease,
                   computeAuthority, moduleSignatureEnforced,
                   signingCertificateBound, kmsReady, relayReady,
                   redistributionAuthorized, distributed, rejected>>

RejectHostSelectedModule ==
    /\ moduleFlavor = Open
    /\ selector = None
    /\ ~rejected
    /\ rejected' = TRUE
    /\ UNCHANGED <<moduleFlavor, moduleRelease, firmwareRelease, selector,
                   computeAuthority, moduleSignatureEnforced,
                   signingCertificateBound, kmsReady, relayReady,
                   redistributionAuthorized, distributed>>

RejectComputeAuthority ==
    /\ moduleFlavor = Open
    /\ ~computeAuthority
    /\ ~rejected
    /\ rejected' = TRUE
    /\ UNCHANGED <<moduleFlavor, moduleRelease, firmwareRelease, selector,
                   computeAuthority, moduleSignatureEnforced,
                   signingCertificateBound, kmsReady, relayReady,
                   redistributionAuthorized, distributed>>

BindModuleSigningPolicy ==
    /\ moduleFlavor = Open
    /\ ~moduleSignatureEnforced
    /\ ~signingCertificateBound
    /\ moduleSignatureEnforced' = TRUE
    /\ signingCertificateBound' = TRUE
    /\ UNCHANGED <<moduleFlavor, moduleRelease, firmwareRelease, selector,
                   computeAuthority, kmsReady, relayReady,
                   redistributionAuthorized, distributed, rejected>>

RejectUnsignedModulePolicy ==
    /\ moduleFlavor = Open
    /\ ~(moduleSignatureEnforced /\ signingCertificateBound)
    /\ ~rejected
    /\ rejected' = TRUE
    /\ UNCHANGED <<moduleFlavor, moduleRelease, firmwareRelease, selector,
                   computeAuthority, moduleSignatureEnforced,
                   signingCertificateBound, kmsReady, relayReady,
                   redistributionAuthorized, distributed>>

InitializeKms ==
    /\ moduleFlavor = Open
    /\ moduleRelease = Pinned
    /\ firmwareRelease = Pinned
    /\ selector = KernelModalias
    /\ ~computeAuthority
    /\ moduleSignatureEnforced
    /\ signingCertificateBound
    /\ ~kmsReady
    /\ kmsReady' = TRUE
    /\ UNCHANGED <<moduleFlavor, moduleRelease, firmwareRelease, selector,
                   computeAuthority, moduleSignatureEnforced,
                   signingCertificateBound, relayReady, redistributionAuthorized,
                   distributed, rejected>>

StartRelay ==
    /\ kmsReady
    /\ ~relayReady
    /\ relayReady' = TRUE
    /\ UNCHANGED <<moduleFlavor, moduleRelease, firmwareRelease, selector,
                   computeAuthority, moduleSignatureEnforced,
                   signingCertificateBound, kmsReady, redistributionAuthorized,
                   distributed, rejected>>

RevokeDisplay ==
    /\ kmsReady \/ relayReady
    /\ kmsReady' = FALSE
    /\ relayReady' = FALSE
    /\ UNCHANGED <<moduleFlavor, moduleRelease, firmwareRelease, selector,
                   computeAuthority, moduleSignatureEnforced,
                   signingCertificateBound, redistributionAuthorized, distributed,
                   rejected>>

AuthorizeRedistribution ==
    /\ ~redistributionAuthorized
    /\ redistributionAuthorized' = TRUE
    /\ UNCHANGED <<moduleFlavor, moduleRelease, firmwareRelease, selector,
                   computeAuthority, moduleSignatureEnforced,
                   signingCertificateBound, kmsReady, relayReady, distributed,
                   rejected>>

DistributeFirmware ==
    /\ redistributionAuthorized
    /\ moduleRelease = Pinned
    /\ firmwareRelease = Pinned
    /\ ~distributed
    /\ distributed' = TRUE
    /\ UNCHANGED <<moduleFlavor, moduleRelease, firmwareRelease, selector,
                   computeAuthority, moduleSignatureEnforced,
                   signingCertificateBound, kmsReady, relayReady,
                   redistributionAuthorized, rejected>>

Next ==
    \/ StagePinnedOpenStack
    \/ RejectMixedOrProprietaryStack
    \/ SelectAssignedPciModalias
    \/ RejectHostSelectedModule
    \/ RejectComputeAuthority
    \/ BindModuleSigningPolicy
    \/ RejectUnsignedModulePolicy
    \/ InitializeKms
    \/ StartRelay
    \/ RevokeDisplay
    \/ AuthorizeRedistribution
    \/ DistributeFirmware

TypeOK ==
    /\ moduleFlavor \in {None, Open}
    /\ moduleRelease \in {None, Pinned}
    /\ firmwareRelease \in {None, Pinned}
    /\ selector \in {None, KernelModalias}
    /\ computeAuthority \in BOOLEAN
    /\ moduleSignatureEnforced \in BOOLEAN
    /\ signingCertificateBound \in BOOLEAN
    /\ kmsReady \in BOOLEAN
    /\ relayReady \in BOOLEAN
    /\ redistributionAuthorized \in BOOLEAN
    /\ distributed \in BOOLEAN
    /\ rejected \in BOOLEAN

KmsRequiresExactOpenStack ==
    kmsReady =>
        /\ moduleFlavor = Open
        /\ moduleRelease = Pinned
        /\ firmwareRelease = Pinned
        /\ selector = KernelModalias
        /\ moduleSignatureEnforced
        /\ signingCertificateBound

RelayRequiresCompleteKms == relayReady => kmsReady
DisplayDvmHasNoComputeAuthority == ~computeAuthority
KmsRequiresSignedModules ==
    kmsReady => moduleSignatureEnforced /\ signingCertificateBound
DistributionRequiresAuthorization == distributed => redistributionAuthorized
DistributedFirmwareMatchesModule ==
    distributed => moduleRelease = firmwareRelease /\ moduleRelease = Pinned

Spec == Init /\ [][Next]_vars

=============================================================================
