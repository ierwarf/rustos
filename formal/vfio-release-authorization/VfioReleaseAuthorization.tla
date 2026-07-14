------------------------- MODULE VfioReleaseAuthorization -----------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models the production VFIO activation gate.

Concrete owners and source anchors:
  * release parser and durable lease: libs/driver-domain-host/src/lib.rs
  * OpenPGP verification and fresh clock checks: tools/hostd/src/main.rs

A topology preflight has no mutation authority. A host device may become
VFIO-bound only after a signature-verified authorization exactly binds its
domain, CID, complete IOMMU group, DVM manifest hash, and device-policy hash.
Every mutating transition rechecks the bounded authorization window. The
durable prepared/active record retains that binding, while restore deliberately
needs no live authorization so an expired release cannot orphan a device.

The model abstracts GPG packet parsing, SHA-256 arithmetic, sysfs writes, and
the contents of the DVM artifact. `signature = Valid` represents the result
of gpgv against the pinned keyring, not a guest assertion.
*******************************************************************************)

CONSTANTS Domains, Cids, Groups, Artifacts, Policies, Manifests,
          ExpectedManifest, ExpectedDomain, ExpectedCid, ExpectedGroup,
          ExpectedArtifact, ExpectedPolicy, MaxTime

NoString == "none"
NoCid == 0
NoGroup == 0
Idle == "idle"
Preflight == "preflight"
Authorized == "authorized"
Prepared == "prepared"
Active == "active"
Valid == "valid"
NoSignature == "none"
Times == 0..MaxTime
AuthKinds == {"exact", "wrong-domain", "wrong-cid", "wrong-group",
              "wrong-artifact", "wrong-policy"}

ForeignDomain == CHOOSE domain \in Domains: domain # ExpectedDomain
ForeignCid == CHOOSE cid \in Cids: cid # ExpectedCid
ForeignGroup == CHOOSE group \in Groups: group # ExpectedGroup
ForeignArtifact == CHOOSE artifact \in Artifacts: artifact # ExpectedArtifact
ForeignPolicy == CHOOSE policy \in Policies: policy # ExpectedPolicy

CandidateDomain(kind) ==
    IF kind = "wrong-domain" THEN ForeignDomain ELSE ExpectedDomain
CandidateCid(kind) ==
    IF kind = "wrong-cid" THEN ForeignCid ELSE ExpectedCid
CandidateGroup(kind) ==
    IF kind = "wrong-group" THEN ForeignGroup ELSE ExpectedGroup
CandidateArtifact(kind) ==
    IF kind = "wrong-artifact" THEN ForeignArtifact ELSE ExpectedArtifact
CandidatePolicy(kind) ==
    IF kind = "wrong-policy" THEN ForeignPolicy ELSE ExpectedPolicy

VARIABLES phase, now,
          leaseDomain, leaseCid, leaseGroup, leaseArtifact, leasePolicy,
          signature, authManifest, authDomain, authCid, authGroup, authArtifact, authPolicy,
          authNotBefore, authNotAfter,
          recordDurable, recordManifest, recordDomain, recordCid, recordGroup,
          recordArtifact, recordPolicy, recordAuthorizedAt, recordNotAfter,
          boundGroup, activationTime, rejected

vars == <<phase, now,
          leaseDomain, leaseCid, leaseGroup, leaseArtifact, leasePolicy,
          signature, authManifest, authDomain, authCid, authGroup, authArtifact, authPolicy,
          authNotBefore, authNotAfter,
          recordDurable, recordManifest, recordDomain, recordCid, recordGroup,
          recordArtifact, recordPolicy, recordAuthorizedAt, recordNotAfter,
          boundGroup, activationTime, rejected>>

ClearAuthorization ==
    /\ signature' = NoSignature
    /\ authManifest' = NoString
    /\ authDomain' = NoString
    /\ authCid' = NoCid
    /\ authGroup' = NoGroup
    /\ authArtifact' = NoString
    /\ authPolicy' = NoString
    /\ authNotBefore' = 0
    /\ authNotAfter' = 0

ClearRecord ==
    /\ recordDurable' = FALSE
    /\ recordManifest' = NoString
    /\ recordDomain' = NoString
    /\ recordCid' = NoCid
    /\ recordGroup' = NoGroup
    /\ recordArtifact' = NoString
    /\ recordPolicy' = NoString
    /\ recordAuthorizedAt' = 0
    /\ recordNotAfter' = 0
    /\ boundGroup' = NoGroup
    /\ activationTime' = 0

Init ==
    /\ phase = Idle
    /\ now = 0
    /\ leaseDomain = NoString
    /\ leaseCid = NoCid
    /\ leaseGroup = NoGroup
    /\ leaseArtifact = NoString
    /\ leasePolicy = NoString
    /\ signature = NoSignature
    /\ authManifest = NoString
    /\ authDomain = NoString
    /\ authCid = NoCid
    /\ authGroup = NoGroup
    /\ authArtifact = NoString
    /\ authPolicy = NoString
    /\ authNotBefore = 0
    /\ authNotAfter = 0
    /\ recordDurable = FALSE
    /\ recordManifest = NoString
    /\ recordDomain = NoString
    /\ recordCid = NoCid
    /\ recordGroup = NoGroup
    /\ recordArtifact = NoString
    /\ recordPolicy = NoString
    /\ recordAuthorizedAt = 0
    /\ recordNotAfter = 0
    /\ boundGroup = NoGroup
    /\ activationTime = 0
    /\ rejected = FALSE

\* The host has one concrete topology plan for this model run.  Candidate
\* authorizations below enumerate the exact release plus one isolated mismatch
\* for each binding field.  The bounded clock additionally covers before,
\* during, and after a valid window without a Cartesian product of duplicates.
PreflightLease ==
    /\ phase = Idle
    /\ phase' = Preflight
    /\ leaseDomain' = ExpectedDomain
    /\ leaseCid' = ExpectedCid
    /\ leaseGroup' = ExpectedGroup
    /\ leaseArtifact' = ExpectedArtifact
    /\ leasePolicy' = ExpectedPolicy
    /\ ClearAuthorization
    /\ ClearRecord
    /\ UNCHANGED <<now, rejected>>

PresentSignedAuthorization(kind, from, until) ==
    /\ phase = Preflight
    /\ signature = NoSignature
    /\ kind \in AuthKinds
    /\ from \in Times
    /\ until \in Times
    /\ from < until
    /\ signature' = Valid
    /\ authManifest' = ExpectedManifest
    /\ authDomain' = CandidateDomain(kind)
    /\ authCid' = CandidateCid(kind)
    /\ authGroup' = CandidateGroup(kind)
    /\ authArtifact' = CandidateArtifact(kind)
    /\ authPolicy' = CandidatePolicy(kind)
    /\ authNotBefore' = from
    /\ authNotAfter' = until
    /\ UNCHANGED <<phase, now, leaseDomain, leaseCid, leaseGroup, leaseArtifact, leasePolicy,
                  recordDurable, recordManifest, recordDomain, recordCid, recordGroup,
                  recordArtifact, recordPolicy, recordAuthorizedAt, recordNotAfter,
                  boundGroup, activationTime, rejected>>

AuthorizationMatchesLease ==
    /\ signature = Valid
    /\ authDomain = leaseDomain
    /\ authCid = leaseCid
    /\ authGroup = leaseGroup
    /\ authArtifact = leaseArtifact
    /\ authPolicy = leasePolicy
    /\ authNotBefore <= now
    /\ now <= authNotAfter

VerifyExactAuthorization ==
    /\ phase = Preflight
    /\ AuthorizationMatchesLease
    /\ phase' = Authorized
    /\ UNCHANGED <<now, leaseDomain, leaseCid, leaseGroup, leaseArtifact, leasePolicy,
                  signature, authManifest, authDomain, authCid, authGroup, authArtifact,
                  authPolicy, authNotBefore, authNotAfter, recordDurable, recordManifest,
                  recordDomain, recordCid, recordGroup, recordArtifact, recordPolicy,
                  recordAuthorizedAt, recordNotAfter, boundGroup, activationTime, rejected>>

RejectAuthorization ==
    /\ phase = Preflight
    /\ signature = Valid
    /\ ~AuthorizationMatchesLease
    /\ phase' = Idle
    /\ rejected' = TRUE
    /\ leaseDomain' = NoString
    /\ leaseCid' = NoCid
    /\ leaseGroup' = NoGroup
    /\ leaseArtifact' = NoString
    /\ leasePolicy' = NoString
    /\ ClearAuthorization
    /\ ClearRecord
    /\ UNCHANGED now

PrepareDurableLease ==
    /\ phase = Authorized
    /\ AuthorizationMatchesLease
    /\ phase' = Prepared
    /\ recordDurable' = TRUE
    /\ recordManifest' = authManifest
    /\ recordDomain' = authDomain
    /\ recordCid' = authCid
    /\ recordGroup' = authGroup
    /\ recordArtifact' = authArtifact
    /\ recordPolicy' = authPolicy
    /\ recordAuthorizedAt' = now
    /\ recordNotAfter' = authNotAfter
    /\ boundGroup' = NoGroup
    /\ activationTime' = 0
    /\ UNCHANGED <<now, leaseDomain, leaseCid, leaseGroup, leaseArtifact, leasePolicy,
                  signature, authManifest, authDomain, authCid, authGroup, authArtifact,
                  authPolicy, authNotBefore, authNotAfter, rejected>>

RecordMatchesLease ==
    /\ recordDurable
    /\ recordManifest = authManifest
    /\ recordDomain = leaseDomain
    /\ recordCid = leaseCid
    /\ recordGroup = leaseGroup
    /\ recordArtifact = leaseArtifact
    /\ recordPolicy = leasePolicy
    /\ recordAuthorizedAt <= now
    /\ now <= recordNotAfter

ActivateVfio ==
    /\ phase = Prepared
    /\ AuthorizationMatchesLease
    /\ RecordMatchesLease
    /\ phase' = Active
    /\ boundGroup' = recordGroup
    /\ activationTime' = now
    /\ UNCHANGED <<now, leaseDomain, leaseCid, leaseGroup, leaseArtifact, leasePolicy,
                  signature, authManifest, authDomain, authCid, authGroup, authArtifact,
                  authPolicy, authNotBefore, authNotAfter, recordDurable, recordManifest,
                  recordDomain, recordCid, recordGroup, recordArtifact, recordPolicy,
                  recordAuthorizedAt, recordNotAfter, rejected>>

RestoreOriginalDrivers ==
    /\ phase \in {Prepared, Active}
    /\ phase' = Idle
    /\ leaseDomain' = NoString
    /\ leaseCid' = NoCid
    /\ leaseGroup' = NoGroup
    /\ leaseArtifact' = NoString
    /\ leasePolicy' = NoString
    /\ ClearAuthorization
    /\ ClearRecord
    /\ UNCHANGED <<now, rejected>>

AbandonUnboundAuthorization ==
    /\ phase \in {Preflight, Authorized}
    /\ phase' = Idle
    /\ leaseDomain' = NoString
    /\ leaseCid' = NoCid
    /\ leaseGroup' = NoGroup
    /\ leaseArtifact' = NoString
    /\ leasePolicy' = NoString
    /\ ClearAuthorization
    /\ ClearRecord
    /\ UNCHANGED <<now, rejected>>

AdvanceTime ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<phase, leaseDomain, leaseCid, leaseGroup, leaseArtifact, leasePolicy,
                  signature, authManifest, authDomain, authCid, authGroup, authArtifact,
                  authPolicy, authNotBefore, authNotAfter, recordDurable, recordManifest,
                  recordDomain, recordCid, recordGroup, recordArtifact, recordPolicy,
                  recordAuthorizedAt, recordNotAfter, boundGroup, activationTime, rejected>>

Next ==
    \/ PreflightLease
    \/ \E kind \in AuthKinds, from \in Times, until \in Times :
          PresentSignedAuthorization(kind, from, until)
    \/ VerifyExactAuthorization
    \/ RejectAuthorization
    \/ PrepareDurableLease
    \/ ActivateVfio
    \/ RestoreOriginalDrivers
    \/ AbandonUnboundAuthorization
    \/ AdvanceTime

TypeOK ==
    /\ ExpectedManifest \in Manifests
    /\ ExpectedDomain \in Domains
    /\ ExpectedCid \in Cids
    /\ ExpectedGroup \in Groups
    /\ ExpectedArtifact \in Artifacts
    /\ ExpectedPolicy \in Policies
    /\ phase \in {Idle, Preflight, Authorized, Prepared, Active}
    /\ now \in Times
    /\ leaseDomain \in Domains \cup {NoString}
    /\ leaseCid \in Cids \cup {NoCid}
    /\ leaseGroup \in Groups \cup {NoGroup}
    /\ leaseArtifact \in Artifacts \cup {NoString}
    /\ leasePolicy \in Policies \cup {NoString}
    /\ signature \in {NoSignature, Valid}
    /\ authManifest \in Manifests \cup {NoString}
    /\ authDomain \in Domains \cup {NoString}
    /\ authCid \in Cids \cup {NoCid}
    /\ authGroup \in Groups \cup {NoGroup}
    /\ authArtifact \in Artifacts \cup {NoString}
    /\ authPolicy \in Policies \cup {NoString}
    /\ authNotBefore \in Times
    /\ authNotAfter \in Times
    /\ recordDurable \in BOOLEAN
    /\ recordManifest \in Manifests \cup {NoString}
    /\ recordDomain \in Domains \cup {NoString}
    /\ recordCid \in Cids \cup {NoCid}
    /\ recordGroup \in Groups \cup {NoGroup}
    /\ recordArtifact \in Artifacts \cup {NoString}
    /\ recordPolicy \in Policies \cup {NoString}
    /\ recordAuthorizedAt \in Times
    /\ recordNotAfter \in Times
    /\ boundGroup \in Groups \cup {NoGroup}
    /\ activationTime \in Times
    /\ rejected \in BOOLEAN

NoUnsignedOrPartialVfio ==
    phase = Active =>
        /\ signature = Valid
        /\ recordDurable
        /\ boundGroup = leaseGroup
        /\ recordDomain = leaseDomain
        /\ recordCid = leaseCid
        /\ recordArtifact = leaseArtifact
        /\ recordPolicy = leasePolicy
        /\ activationTime >= authNotBefore
        /\ activationTime <= recordNotAfter

EveryMutationWasWithinAuthorization ==
    phase \in {Prepared, Active} =>
        /\ recordAuthorizedAt >= authNotBefore
        /\ recordAuthorizedAt <= recordNotAfter
        /\ recordNotAfter = authNotAfter

IdleHasNoDeviceAuthority ==
    phase = Idle => /\ ~recordDurable /\ boundGroup = NoGroup

Spec == Init /\ [][Next]_vars
=============================================================================
