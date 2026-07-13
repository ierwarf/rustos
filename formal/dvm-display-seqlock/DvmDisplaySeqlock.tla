------------------------------ MODULE DvmDisplaySeqlock ---------------------------
EXTENDS Naturals

(*******************************************************************************
Models the DVM shared-display generation protocol.

Concrete owners:
  * kernel/io-manager/src/io/dvm_display.rs
  * kernel/io-manager/src/io/gui/backend.rs

A DVM present changes the shared generation from even to odd while it writes,
then back to even before releasing DISPLAY_BACKEND. A provider replacement
uses that same lock before detaching the DVM header. The retired header must
therefore always retain an even generation; otherwise the DVM DRM/KMS relay can
wait forever for a frame completion that the kernel can no longer publish.
*******************************************************************************)

NoProvider == "none"
DvmProvider == "dvm"
OtherProvider == "other"

Free == "free"
Held == "held"

EvenGenerations == {2, 4, 6}
OddGenerations == {3, 5, 7}

NextOdd(generation) ==
    IF generation = 2 THEN 3
    ELSE IF generation = 4 THEN 5
    ELSE 7

NextEven(generation) ==
    IF generation = 3 THEN 4
    ELSE IF generation = 5 THEN 6
    ELSE 2

VARIABLES provider,
          backendLock,
          sharedHeaderAttached,
          sharedGeneration,
          frameInProgress,
          retiredGeneration

vars == <<provider, backendLock, sharedHeaderAttached, sharedGeneration,
          frameInProgress, retiredGeneration>>

Init ==
    /\ provider = NoProvider
    /\ backendLock = Free
    /\ sharedHeaderAttached = FALSE
    /\ sharedGeneration = 0
    /\ frameInProgress = FALSE
    /\ retiredGeneration = 0

AttachDvmProvider ==
    /\ provider \in {NoProvider, OtherProvider}
    /\ backendLock = Free
    /\ frameInProgress = FALSE
    /\ provider' = DvmProvider
    /\ sharedHeaderAttached' = TRUE
    /\ sharedGeneration' = 2
    /\ retiredGeneration' = 0
    /\ UNCHANGED <<backendLock, frameInProgress>>

BeginFrame ==
    /\ provider = DvmProvider
    /\ sharedHeaderAttached
    /\ backendLock = Free
    /\ frameInProgress = FALSE
    /\ sharedGeneration \in EvenGenerations
    /\ backendLock' = Held
    /\ frameInProgress' = TRUE
    /\ sharedGeneration' = NextOdd(sharedGeneration)
    /\ UNCHANGED <<provider, sharedHeaderAttached, retiredGeneration>>

FinishFrame ==
    /\ provider = DvmProvider
    /\ sharedHeaderAttached
    /\ backendLock = Held
    /\ frameInProgress
    /\ sharedGeneration \in OddGenerations
    /\ backendLock' = Free
    /\ frameInProgress' = FALSE
    /\ sharedGeneration' = NextEven(sharedGeneration)
    /\ UNCHANGED <<provider, sharedHeaderAttached, retiredGeneration>>

(*******************************************************************************
Corresponds to install_driver_framebuffer taking DISPLAY_BACKEND before
on_framebuffer_installed. The guard is the critical source-level contract:
replacement cannot detach a DVM header from an in-flight frame.
*******************************************************************************)
ReplaceDvmProvider ==
    /\ provider = DvmProvider
    /\ backendLock = Free
    /\ frameInProgress = FALSE
    /\ sharedHeaderAttached
    /\ sharedGeneration \in EvenGenerations
    /\ provider' = OtherProvider
    /\ sharedHeaderAttached' = FALSE
    /\ sharedGeneration' = 0
    /\ retiredGeneration' = sharedGeneration
    /\ UNCHANGED <<backendLock, frameInProgress>>

Next ==
    \/ AttachDvmProvider
    \/ BeginFrame
    \/ FinishFrame
    \/ ReplaceDvmProvider

TypeOK ==
    /\ provider \in {NoProvider, DvmProvider, OtherProvider}
    /\ backendLock \in {Free, Held}
    /\ sharedHeaderAttached \in BOOLEAN
    /\ sharedGeneration \in EvenGenerations \cup OddGenerations \cup {0}
    /\ frameInProgress \in BOOLEAN
    /\ retiredGeneration \in EvenGenerations \cup {0}

BackendLockSerializesFrameLifetime ==
    backendLock = Held <=> frameInProgress

DvmHeaderExactlyMatchesProvider ==
    /\ provider = DvmProvider =>
        /\ sharedHeaderAttached
        /\ sharedGeneration # 0
    /\ provider # DvmProvider =>
        /\ ~sharedHeaderAttached
        /\ sharedGeneration = 0
        /\ ~frameInProgress

AttachedHeaderParityMatchesFrameState ==
    sharedHeaderAttached =>
        IF frameInProgress
        THEN sharedGeneration \in OddGenerations
        ELSE sharedGeneration \in EvenGenerations

RetiredDvmHeaderIsNeverInProgress ==
    retiredGeneration = 0 \/ retiredGeneration \in EvenGenerations

FrameCannotOutliveDvmProvider ==
    frameInProgress => provider = DvmProvider /\ sharedHeaderAttached

=============================================================================
