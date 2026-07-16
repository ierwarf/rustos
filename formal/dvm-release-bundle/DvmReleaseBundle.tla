------------------------- MODULE DvmReleaseBundle -------------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Self-contained Linux DVM release-bundle admission contract.

Concrete owners:
  driver-domains/linux/scripts/write-manifest.sh
  driver-domains/linux/scripts/verify-release-artifacts.sh
  driver-domains/linux/scripts/stage-release.sh
  tools/hostd/src/runtime.rs
  tools/xtask/src/kvm.rs

The bundle contains exactly one strict 25-key manifest, a strict six-key
control contract, and six other named payload files.  The staging linearization
point is the rename of a completely copied,
fsync'd, reverified temporary directory to a fresh destination.  Host launch
authority linearizes only after hostd independently verifies the published
bundle.  Unsafe ancestors, a pre-existing destination, missing/unknown schema
state, any companion hash mismatch, and post-publication mutation fail closed.
An attempted replacement cannot change the published bundle.
***************************************************************************)

Unstaged == "unstaged"
Copying == "copying"
Published == "published"
Verified == "verified"
Rejected == "rejected"
Launched == "launched"

Components == {
    "manifest",
    "kernel",
    "rootfs",
    "buildroot-config",
    "kernel-config",
    "signing-cert",
    "sources-lock",
    "control-contract"
}

VARIABLES phase,
          sourceVerified,
          schemaExact,
          safeAncestors,
          destinationFresh,
          coLocated,
          copied,
          hashMatched,
          immutablePublished,
          hostVerified,
          replacementAttempted,
          mutationObserved,
          launchAuthority

vars == <<phase, sourceVerified, schemaExact, safeAncestors,
          destinationFresh, coLocated, copied, hashMatched,
          immutablePublished, hostVerified, replacementAttempted,
          mutationObserved, launchAuthority>>

Init ==
    /\ phase = Unstaged
    /\ sourceVerified = FALSE
    /\ schemaExact = FALSE
    /\ safeAncestors = FALSE
    /\ destinationFresh = FALSE
    /\ coLocated = FALSE
    /\ copied = {}
    /\ hashMatched = {}
    /\ immutablePublished = FALSE
    /\ hostVerified = FALSE
    /\ replacementAttempted = FALSE
    /\ mutationObserved = FALSE
    /\ launchAuthority = FALSE

VerifyExactSource ==
    /\ phase = Unstaged
    /\ ~sourceVerified
    /\ sourceVerified' = TRUE
    /\ schemaExact' = TRUE
    /\ UNCHANGED <<phase, safeAncestors, destinationFresh, coLocated,
                  copied, hashMatched, immutablePublished, hostVerified,
                  replacementAttempted, mutationObserved, launchAuthority>>

RejectInvalidSource ==
    /\ phase = Unstaged
    /\ ~sourceVerified
    /\ phase' = Rejected
    /\ UNCHANGED <<sourceVerified, schemaExact, safeAncestors,
                  destinationFresh, coLocated, copied, hashMatched,
                  immutablePublished, hostVerified, replacementAttempted,
                  mutationObserved, launchAuthority>>

BeginSafeStage ==
    /\ phase = Unstaged
    /\ sourceVerified
    /\ schemaExact
    /\ phase' = Copying
    /\ safeAncestors' = TRUE
    /\ destinationFresh' = TRUE
    /\ coLocated' = TRUE
    /\ UNCHANGED <<sourceVerified, schemaExact, copied, hashMatched,
                  immutablePublished, hostVerified, replacementAttempted,
                  mutationObserved, launchAuthority>>

RejectUnsafeOrExistingDestination ==
    /\ phase = Unstaged
    /\ sourceVerified
    /\ phase' = Rejected
    /\ UNCHANGED <<sourceVerified, schemaExact, safeAncestors,
                  destinationFresh, coLocated, copied, hashMatched,
                  immutablePublished, hostVerified, replacementAttempted,
                  mutationObserved, launchAuthority>>

CopyValidComponent(component) ==
    /\ phase = Copying
    /\ component \in Components \ copied
    /\ copied' = copied \cup {component}
    /\ hashMatched' = hashMatched \cup {component}
    /\ UNCHANGED <<phase, sourceVerified, schemaExact, safeAncestors,
                  destinationFresh, coLocated, immutablePublished,
                  hostVerified, replacementAttempted, mutationObserved,
                  launchAuthority>>

CopyCorruptComponent(component) ==
    /\ phase = Copying
    /\ component \in Components \ copied
    /\ copied' = copied \cup {component}
    /\ mutationObserved' = TRUE
    /\ UNCHANGED <<phase, sourceVerified, schemaExact, safeAncestors,
                  destinationFresh, coLocated, hashMatched,
                  immutablePublished, hostVerified, replacementAttempted,
                  launchAuthority>>

RejectIncompleteOrCorruptCopy ==
    /\ phase = Copying
    /\ copied = Components
    /\ hashMatched # Components
    /\ phase' = Rejected
    /\ UNCHANGED <<sourceVerified, schemaExact, safeAncestors,
                  destinationFresh, coLocated, copied, hashMatched,
                  immutablePublished, hostVerified, replacementAttempted,
                  mutationObserved, launchAuthority>>

PublishAtomicRename ==
    /\ phase = Copying
    /\ sourceVerified
    /\ schemaExact
    /\ safeAncestors
    /\ destinationFresh
    /\ coLocated
    /\ copied = Components
    /\ hashMatched = Components
    /\ phase' = Published
    /\ immutablePublished' = TRUE
    /\ UNCHANGED <<sourceVerified, schemaExact, safeAncestors,
                  destinationFresh, coLocated, copied, hashMatched,
                  hostVerified, replacementAttempted, mutationObserved,
                  launchAuthority>>

VerifyPublishedBundle ==
    /\ phase = Published
    /\ immutablePublished
    /\ copied = Components
    /\ hashMatched = Components
    /\ phase' = Verified
    /\ hostVerified' = TRUE
    /\ UNCHANGED <<sourceVerified, schemaExact, safeAncestors,
                  destinationFresh, coLocated, copied, hashMatched,
                  immutablePublished, replacementAttempted,
                  mutationObserved, launchAuthority>>

AttemptReplacement ==
    /\ phase \in {Published, Verified}
    /\ ~replacementAttempted
    /\ replacementAttempted' = TRUE
    /\ UNCHANGED <<phase, sourceVerified, schemaExact, safeAncestors,
                  destinationFresh, coLocated, copied, hashMatched,
                  immutablePublished, hostVerified, mutationObserved,
                  launchAuthority>>

ObservePostPublicationMutation ==
    /\ phase \in {Published, Verified}
    /\ phase' = Rejected
    /\ immutablePublished' = FALSE
    /\ hostVerified' = FALSE
    /\ mutationObserved' = TRUE
    /\ UNCHANGED <<sourceVerified, schemaExact, safeAncestors,
                  destinationFresh, coLocated, copied, hashMatched,
                  replacementAttempted, launchAuthority>>

GrantLaunchAuthority ==
    /\ phase = Verified
    /\ hostVerified
    /\ immutablePublished
    /\ ~mutationObserved
    /\ phase' = Launched
    /\ launchAuthority' = TRUE
    /\ UNCHANGED <<sourceVerified, schemaExact, safeAncestors,
                  destinationFresh, coLocated, copied, hashMatched,
                  immutablePublished, hostVerified, replacementAttempted,
                  mutationObserved>>

Next ==
    \/ VerifyExactSource
    \/ RejectInvalidSource
    \/ BeginSafeStage
    \/ RejectUnsafeOrExistingDestination
    \/ \E component \in Components: CopyValidComponent(component)
    \/ \E component \in Components: CopyCorruptComponent(component)
    \/ RejectIncompleteOrCorruptCopy
    \/ PublishAtomicRename
    \/ VerifyPublishedBundle
    \/ AttemptReplacement
    \/ ObservePostPublicationMutation
    \/ GrantLaunchAuthority

TypeOK ==
    /\ phase \in {Unstaged, Copying, Published, Verified, Rejected, Launched}
    /\ sourceVerified \in BOOLEAN
    /\ schemaExact \in BOOLEAN
    /\ safeAncestors \in BOOLEAN
    /\ destinationFresh \in BOOLEAN
    /\ coLocated \in BOOLEAN
    /\ copied \subseteq Components
    /\ hashMatched \subseteq Components
    /\ immutablePublished \in BOOLEAN
    /\ hostVerified \in BOOLEAN
    /\ replacementAttempted \in BOOLEAN
    /\ mutationObserved \in BOOLEAN
    /\ launchAuthority \in BOOLEAN

PublishedBundleIsComplete ==
    phase \in {Published, Verified, Launched} =>
        /\ sourceVerified
        /\ schemaExact
        /\ safeAncestors
        /\ destinationFresh
        /\ coLocated
        /\ copied = Components
        /\ hashMatched = Components

HostVerificationRequiresImmutablePublication ==
    hostVerified =>
        /\ phase \in {Verified, Launched}
        /\ immutablePublished
        /\ ~mutationObserved

LaunchRequiresCompleteIndependentAdmission ==
    launchAuthority =>
        /\ phase = Launched
        /\ hostVerified
        /\ immutablePublished
        /\ copied = Components
        /\ hashMatched = Components
        /\ schemaExact
        /\ safeAncestors
        /\ destinationFresh
        /\ coLocated
        /\ ~mutationObserved

RejectedBundleHasNoLaunchAuthority ==
    phase = Rejected => ~launchAuthority

ReplacementDoesNotChangePublishedBundle ==
    replacementAttempted /\ phase \in {Published, Verified} =>
        /\ immutablePublished
        /\ copied = Components
        /\ hashMatched = Components

Spec == Init /\ [][Next]_vars

=============================================================================
