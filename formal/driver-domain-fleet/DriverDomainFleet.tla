--------------------------- MODULE DriverDomainFleet --------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models the fleet-level exclusivity contract for hardware-backed driver domains.

Concrete owners and source anchors:
  * strict fleet parsing and exact member lookup:
    libs/driver-domain-host/src/lib.rs DriverDomainFleetPolicy
  * pinned-key release verification and fleet hash comparison:
    tools/hostd/src/main.rs verify_release_authorization

Each model member represents one complete IOMMU group by one representative
PCI BDF.  The implementation validates every BDF in each member; the model's
single representative captures the cross-member alias relation.  It does not
model DMA or guest behaviour.
*******************************************************************************)

CONSTANTS Domains, Cids, Groups, Bdfs, Hashes, FleetHash

NoString == "none"
NoCid == 0
NoGroup == 0
Configuring == "configuring"
Candidate == "candidate"
Idle == "idle"
ReleasePresented == "release-presented"
Active == "active"
Valid == "valid"
Invalid == "invalid"

VARIABLES phase, sealed,
          memberCid, memberGroup, memberBdf,
          candidateDomain, candidateCid, candidateGroup, candidateBdf,
          releaseDomain, releaseHash, releaseSignature, activeDomain

vars == <<phase, sealed,
          memberCid, memberGroup, memberBdf,
          candidateDomain, candidateCid, candidateGroup, candidateBdf,
          releaseDomain, releaseHash, releaseSignature, activeDomain>>

MemberKnown(domain) ==
    memberCid[domain] # NoCid /\ memberGroup[domain] # NoGroup /\ memberBdf[domain] # NoString

CandidateConflicts ==
    MemberKnown(candidateDomain)
    \/ \E other \in Domains:
          other # candidateDomain /\ MemberKnown(other)
          /\ (memberCid[other] = candidateCid
              \/ memberGroup[other] = candidateGroup
              \/ memberBdf[other] = candidateBdf)

ClearCandidate ==
    /\ candidateDomain' = NoString
    /\ candidateCid' = NoCid
    /\ candidateGroup' = NoGroup
    /\ candidateBdf' = NoString

ClearRelease ==
    /\ releaseDomain' = NoString
    /\ releaseHash' = NoString
    /\ releaseSignature' = Invalid

Init ==
    /\ phase = Configuring
    /\ sealed = FALSE
    /\ memberCid = [domain \in Domains |-> NoCid]
    /\ memberGroup = [domain \in Domains |-> NoGroup]
    /\ memberBdf = [domain \in Domains |-> NoString]
    /\ candidateDomain = NoString
    /\ candidateCid = NoCid
    /\ candidateGroup = NoGroup
    /\ candidateBdf = NoString
    /\ releaseDomain = NoString
    /\ releaseHash = NoString
    /\ releaseSignature = Invalid
    /\ activeDomain = NoString

StageCandidate(domain, cid, group, bdf) ==
    /\ phase = Configuring
    /\ ~sealed
    /\ domain \in Domains
    /\ cid \in Cids
    /\ group \in Groups
    /\ bdf \in Bdfs
    /\ phase' = Candidate
    /\ candidateDomain' = domain
    /\ candidateCid' = cid
    /\ candidateGroup' = group
    /\ candidateBdf' = bdf
    /\ UNCHANGED <<sealed, memberCid, memberGroup, memberBdf,
                  releaseDomain, releaseHash, releaseSignature, activeDomain>>

AdmitCandidate ==
    /\ phase = Candidate
    /\ ~CandidateConflicts
    /\ phase' = Configuring
    /\ memberCid' = [memberCid EXCEPT ![candidateDomain] = candidateCid]
    /\ memberGroup' = [memberGroup EXCEPT ![candidateDomain] = candidateGroup]
    /\ memberBdf' = [memberBdf EXCEPT ![candidateDomain] = candidateBdf]
    /\ ClearCandidate
    /\ UNCHANGED <<sealed, releaseDomain, releaseHash, releaseSignature, activeDomain>>

RejectCandidate ==
    /\ phase = Candidate
    /\ CandidateConflicts
    /\ phase' = Configuring
    /\ ClearCandidate
    /\ UNCHANGED <<sealed, memberCid, memberGroup, memberBdf,
                  releaseDomain, releaseHash, releaseSignature, activeDomain>>

SealFleet ==
    /\ phase = Configuring
    /\ ~sealed
    /\ \E domain \in Domains: MemberKnown(domain)
    /\ phase' = Idle
    /\ sealed' = TRUE
    /\ UNCHANGED <<memberCid, memberGroup, memberBdf,
                  candidateDomain, candidateCid, candidateGroup, candidateBdf,
                  releaseDomain, releaseHash, releaseSignature, activeDomain>>

PresentRelease(domain, hash, signature) ==
    /\ phase = Idle
    /\ sealed
    /\ domain \in Domains
    /\ MemberKnown(domain)
    /\ hash \in Hashes
    /\ signature \in {Valid, Invalid}
    /\ phase' = ReleasePresented
    /\ releaseDomain' = domain
    /\ releaseHash' = hash
    /\ releaseSignature' = signature
    /\ UNCHANGED <<sealed, memberCid, memberGroup, memberBdf,
                  candidateDomain, candidateCid, candidateGroup, candidateBdf, activeDomain>>

ActivateMember ==
    /\ phase = ReleasePresented
    /\ sealed
    /\ releaseSignature = Valid
    /\ releaseHash = FleetHash
    /\ MemberKnown(releaseDomain)
    /\ phase' = Active
    /\ activeDomain' = releaseDomain
    /\ UNCHANGED <<sealed, memberCid, memberGroup, memberBdf,
                  candidateDomain, candidateCid, candidateGroup, candidateBdf,
                  releaseDomain, releaseHash, releaseSignature>>

RejectRelease ==
    /\ phase = ReleasePresented
    /\ (releaseSignature = Invalid \/ releaseHash # FleetHash)
    /\ phase' = Idle
    /\ ClearRelease
    /\ UNCHANGED <<sealed, memberCid, memberGroup, memberBdf,
                  candidateDomain, candidateCid, candidateGroup, candidateBdf, activeDomain>>

DeactivateMember ==
    /\ phase = Active
    /\ phase' = Idle
    /\ activeDomain' = NoString
    /\ ClearRelease
    /\ UNCHANGED <<sealed, memberCid, memberGroup, memberBdf,
                  candidateDomain, candidateCid, candidateGroup, candidateBdf>>

AbandonRelease ==
    /\ phase = ReleasePresented
    /\ phase' = Idle
    /\ ClearRelease
    /\ UNCHANGED <<sealed, memberCid, memberGroup, memberBdf,
                  candidateDomain, candidateCid, candidateGroup, candidateBdf, activeDomain>>

Next ==
    \/ \E domain \in Domains, cid \in Cids, group \in Groups, bdf \in Bdfs:
          StageCandidate(domain, cid, group, bdf)
    \/ AdmitCandidate
    \/ RejectCandidate
    \/ SealFleet
    \/ \E domain \in Domains, hash \in Hashes, signature \in {Valid, Invalid}:
          PresentRelease(domain, hash, signature)
    \/ ActivateMember
    \/ RejectRelease
    \/ DeactivateMember
    \/ AbandonRelease

TypeOK ==
    /\ phase \in {Configuring, Candidate, Idle, ReleasePresented, Active}
    /\ sealed \in BOOLEAN
    /\ memberCid \in [Domains -> (Cids \cup {NoCid})]
    /\ memberGroup \in [Domains -> (Groups \cup {NoGroup})]
    /\ memberBdf \in [Domains -> (Bdfs \cup {NoString})]
    /\ candidateDomain \in Domains \cup {NoString}
    /\ candidateCid \in Cids \cup {NoCid}
    /\ candidateGroup \in Groups \cup {NoGroup}
    /\ candidateBdf \in Bdfs \cup {NoString}
    /\ releaseDomain \in Domains \cup {NoString}
    /\ releaseHash \in Hashes \cup {NoString}
    /\ releaseSignature \in {Valid, Invalid}
    /\ activeDomain \in Domains \cup {NoString}

CompleteMemberEncoding ==
    \A domain \in Domains:
        MemberKnown(domain)
        \/ /\ memberCid[domain] = NoCid
           /\ memberGroup[domain] = NoGroup
           /\ memberBdf[domain] = NoString

FleetAuthorityIsDisjoint ==
    \A first \in Domains, second \in Domains:
        first # second /\ MemberKnown(first) /\ MemberKnown(second) =>
            /\ memberCid[first] # memberCid[second]
            /\ memberGroup[first] # memberGroup[second]
            /\ memberBdf[first] # memberBdf[second]

ActiveMemberHasSignedSealedFleet ==
    phase = Active =>
        /\ sealed
        /\ activeDomain = releaseDomain
        /\ MemberKnown(activeDomain)
        /\ releaseSignature = Valid
        /\ releaseHash = FleetHash

UnsealedOrStagedStateHasNoDeviceAuthority ==
    /\ ~sealed => activeDomain = NoString
    /\ phase \in {Configuring, Candidate} => activeDomain = NoString

Spec == Init /\ [][Next]_vars
=============================================================================
