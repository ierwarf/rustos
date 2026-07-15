----------------------------- MODULE GuiDvmInstall ---------------------------
EXTENDS Naturals

(*******************************************************************************
Commercial GUI-DVM installation and failure-cleanup model.

Concrete source anchors:
  * kernel/io-manager/src/io/dvm_display.rs try_install
  * kernel/io-manager/src/io/dvm_display.rs arm_gui_dvm_interrupts
  * kernel/io-manager/src/io/dvm_display.rs find_ivshmem_gui_pool

The V3 surface-pool model covers frame ownership after publication. This model
covers the earlier resource boundary: one serialized installer owns the two
MMIO mappings, the two permanent MSI-X vectors, and framebuffer registration.
A malformed or unavailable pool may use the bounded retry budget; an arm or
provider-registration failure fails closed, releases both mappings, and cannot
reserve additional vectors. A live provider can be transport-revoked but is
not silently replaced by a fallback.
*******************************************************************************)

CONSTANT MaxAttachAttempts, MaxConcurrentAttempts

Idle == "idle"
Mapped == "mapped"
Armed == "armed"
Installed == "installed"
Phases == {Idle, Mapped, Armed, Installed}

VARIABLES phase, installRejected, attachAttempts, msixVectors, mmioMappings,
          framebufferRegistered, transportRevoked, concurrentAttempts

vars == <<phase, installRejected, attachAttempts, msixVectors, mmioMappings,
          framebufferRegistered, transportRevoked, concurrentAttempts>>

Init ==
    /\ phase = Idle
    /\ installRejected = FALSE
    /\ attachAttempts = 0
    /\ msixVectors = 0
    /\ mmioMappings = 0
    /\ framebufferRegistered = FALSE
    /\ transportRevoked = FALSE
    /\ concurrentAttempts = 0

\* The compare_exchange install latch grants exactly one caller the mutable
\* installation phase. It owns both BAR mappings before any vector is armed.
BeginAttach ==
    /\ phase = Idle
    /\ ~installRejected
    /\ attachAttempts < MaxAttachAttempts
    /\ phase' = Mapped
    /\ attachAttempts' = attachAttempts + 1
    /\ mmioMappings' = 2
    /\ UNCHANGED <<installRejected, msixVectors, framebufferRegistered,
                  transportRevoked, concurrentAttempts>>

\* A second init/present caller observes the latch and cannot map, allocate,
\* or register a competing provider.
ConcurrentInstallAttempt ==
    /\ phase \in {Mapped, Armed}
    /\ concurrentAttempts < MaxConcurrentAttempts
    /\ concurrentAttempts' = concurrentAttempts + 1
    /\ UNCHANGED <<phase, installRejected, attachAttempts, msixVectors,
                  mmioMappings, framebufferRegistered, transportRevoked>>

MalformedOrAbsentPool ==
    /\ phase = Mapped
    /\ phase' = Idle
    /\ mmioMappings' = 0
    /\ UNCHANGED <<installRejected, attachAttempts, msixVectors,
                  framebufferRegistered, transportRevoked, concurrentAttempts>>

\* `arm_gui_dvm_interrupts` may allocate two permanent vectors only once.
ArmMsix ==
    /\ phase = Mapped
    /\ msixVectors = 0
    /\ phase' = Armed
    /\ msixVectors' = 2
    /\ UNCHANGED <<installRejected, attachAttempts, mmioMappings,
                  framebufferRegistered, transportRevoked, concurrentAttempts>>

\* A late arm error can follow partial permanent-vector reservation. The
\* installation is terminally rejected, but mappings are still released.
ArmFailure ==
    /\ phase = Mapped
    /\ phase' = Idle
    /\ installRejected' = TRUE
    /\ msixVectors' = 2
    /\ mmioMappings' = 0
    /\ UNCHANGED <<attachAttempts, framebufferRegistered, transportRevoked,
                  concurrentAttempts>>

RegisterProvider ==
    /\ phase = Armed
    /\ phase' = Installed
    /\ framebufferRegistered' = TRUE
    /\ UNCHANGED <<installRejected, attachAttempts, msixVectors, mmioMappings,
                  transportRevoked, concurrentAttempts>>

ProviderFailure ==
    /\ phase = Armed
    /\ phase' = Idle
    /\ installRejected' = TRUE
    /\ mmioMappings' = 0
    /\ framebufferRegistered' = FALSE
    /\ UNCHANGED <<attachAttempts, msixVectors, transportRevoked,
                  concurrentAttempts>>

RetryBudgetExhausted ==
    /\ phase = Idle
    /\ ~installRejected
    /\ attachAttempts = MaxAttachAttempts
    /\ installRejected' = TRUE
    /\ UNCHANGED <<phase, attachAttempts, msixVectors, mmioMappings,
                  framebufferRegistered, transportRevoked, concurrentAttempts>>

\* A trusted-display status cannot survive provider loss. The installed V3
\* pool remains the only accepted provider; normal presentation becomes
\* unavailable rather than picking a firmware/native fallback.
RevokeTransport ==
    /\ phase = Installed
    /\ ~transportRevoked
    /\ transportRevoked' = TRUE
    /\ UNCHANGED <<phase, installRejected, attachAttempts, msixVectors,
                  mmioMappings, framebufferRegistered, concurrentAttempts>>

Next ==
    \/ BeginAttach
    \/ ConcurrentInstallAttempt
    \/ MalformedOrAbsentPool
    \/ ArmMsix
    \/ ArmFailure
    \/ RegisterProvider
    \/ ProviderFailure
    \/ RetryBudgetExhausted
    \/ RevokeTransport

TypeOK ==
    /\ phase \in Phases
    /\ installRejected \in BOOLEAN
    /\ attachAttempts \in 0..MaxAttachAttempts
    /\ msixVectors \in 0..2
    /\ mmioMappings \in 0..2
    /\ framebufferRegistered \in BOOLEAN
    /\ transportRevoked \in BOOLEAN
    /\ concurrentAttempts \in 0..MaxConcurrentAttempts

InstallerOwnsExactlyTwoMappings ==
    /\ phase \in {Mapped, Armed, Installed} => mmioMappings = 2
    /\ phase = Idle => mmioMappings = 0

PermanentVectorsAreBounded ==
    /\ msixVectors <= 2
    /\ msixVectors = 2 => phase \in {Armed, Installed, Idle}

RejectedInstallReleasesMappings ==
    installRejected => /\ phase = Idle /\ mmioMappings = 0 /\ ~framebufferRegistered

FramebufferHasOnlyTheV3Provider ==
    framebufferRegistered => /\ phase = Installed /\ mmioMappings = 2 /\ msixVectors = 2

RevokedDisplayCannotFallback ==
    transportRevoked => /\ phase = Installed /\ framebufferRegistered

ConcurrentCallerCannotAllocate ==
    concurrentAttempts > 0 => msixVectors <= 2 /\ mmioMappings <= 2

ResolveMapped == MalformedOrAbsentPool \/ ArmMsix \/ ArmFailure
ResolveArmed == RegisterProvider \/ ProviderFailure

Spec == Init /\ [][Next]_vars
        /\ WF_vars(ResolveMapped)
        /\ WF_vars(ResolveArmed)

AttachmentEventuallySettles ==
    (phase \in {Mapped, Armed}) ~> (phase \in {Idle, Installed})
=============================================================================
