-------------------------- MODULE MmActivationClose --------------------------
EXTENDS Naturals
CONSTANT MaxGeneration
\* One local CPU claim/architecture/RSP transaction plus one remote grant on
\* the same MM. Sequential atomic admission/close, not weak-memory, hardware,
\* physical refs/retirement, funding, full lifecycle/remote-quiescence refinement.
VARIABLES phase, grant, remoteGrant, closed, owner, gen, snapshot,
          queued, leaf, incomingRsp, badPublication, prematureRelease
vars == <<phase, grant, remoteGrant, closed, owner, gen, snapshot,
          queued, leaf, incomingRsp, badPublication, prematureRelease>>
Init == /\ phase = "Idle" /\ grant = FALSE /\ remoteGrant = FALSE
        /\ closed = FALSE /\ owner \in {"Local", "Direct", "Running"}
        /\ gen = 1 /\ snapshot = 1 /\ queued = (owner = "Local")
        /\ leaf = FALSE /\ incomingRsp = FALSE
        /\ badPublication = FALSE /\ prematureRelease = FALSE
CaptureClaim ==
    /\ phase = "Idle" /\ ~closed /\ owner \in {"Local", "Direct"}
    /\ grant' = TRUE /\ leaf' = TRUE /\ phase' = "Claiming" /\ snapshot' = gen
    /\ UNCHANGED <<remoteGrant, closed, owner, gen, queued, incomingRsp,
                    badPublication, prematureRelease>>
Republish ==
    /\ gen < MaxGeneration /\ gen' = gen + 1
    /\ UNCHANGED <<phase, grant, remoteGrant, closed, owner, snapshot,
                    queued, leaf, incomingRsp, badPublication, prematureRelease>>
Claim ==
    /\ phase = "Claiming" /\ leaf /\ snapshot = gen /\ gen < MaxGeneration
    /\ owner' = "Running" /\ gen' = gen + 1 /\ snapshot' = gen + 1
    /\ queued' = FALSE /\ leaf' = FALSE /\ phase' = "Claimed"
    /\ UNCHANGED <<grant, remoteGrant, closed, incomingRsp, badPublication, prematureRelease>>
RefuseClaim ==
    /\ phase = "Claiming" /\ (snapshot # gen \/ gen = MaxGeneration)
    /\ phase' = "Idle" /\ grant' = FALSE /\ leaf' = FALSE
    /\ UNCHANGED <<remoteGrant, closed, owner, gen, snapshot, queued,
                    incomingRsp, badPublication, prematureRelease>>
CaptureCurrent ==
    /\ phase = "Idle" /\ ~closed /\ owner = "Running"
    /\ grant' = TRUE /\ phase' = "Architecture" /\ snapshot' = gen
    /\ UNCHANGED <<remoteGrant, closed, owner, gen, queued, leaf, incomingRsp,
                    badPublication, prematureRelease>>
Prepare ==
    /\ phase = "Claimed" /\ snapshot = gen /\ ~closed
    /\ phase' = "Architecture"
    /\ UNCHANGED <<grant, remoteGrant, closed, owner, gen, snapshot, queued,
                    leaf, incomingRsp, badPublication, prematureRelease>>
Publish ==
    /\ phase = "Architecture" /\ snapshot = gen /\ ~closed
    /\ phase' = "Publishing" /\ badPublication' = (closed \/ snapshot # gen)
    /\ UNCHANGED <<grant, remoteGrant, closed, owner, gen, snapshot, queued,
                    leaf, incomingRsp, prematureRelease>>
CompleteSame ==
    /\ phase = "Publishing" /\ phase' = "Done" /\ grant' = FALSE
    /\ UNCHANGED <<remoteGrant, closed, owner, gen, snapshot, queued, leaf,
                    incomingRsp, badPublication, prematureRelease>>
CompleteChanged ==
    /\ phase = "Publishing" /\ phase' = "AwaitingRsp" /\ grant' = grant
    /\ UNCHANGED <<remoteGrant, closed, owner, gen, snapshot, queued,
                    leaf, incomingRsp, badPublication, prematureRelease>>
AckRsp ==
    /\ phase = "AwaitingRsp" /\ phase' = "Committing" /\ incomingRsp' = TRUE
    /\ UNCHANGED <<grant, remoteGrant, closed, owner, gen, snapshot, queued,
                    leaf, badPublication, prematureRelease>>
Release ==
    /\ phase = "Committing" /\ incomingRsp
    /\ phase' = "Done" /\ grant' = FALSE /\ prematureRelease' = (~incomingRsp)
    /\ UNCHANGED <<remoteGrant, closed, owner, gen, snapshot, queued,
                    leaf, incomingRsp, badPublication>>
Poison ==
    /\ phase \in {"Claimed", "Architecture", "Publishing"}
    /\ phase' = "Poisoned"
    /\ UNCHANGED <<grant, remoteGrant, closed, owner, gen, snapshot, queued,
                    leaf, incomingRsp, badPublication, prematureRelease>>
RemoteCapture ==
    /\ ~remoteGrant /\ ~closed /\ remoteGrant' = TRUE
    /\ UNCHANGED <<phase, grant, closed, owner, gen, snapshot, queued, leaf,
                    incomingRsp, badPublication, prematureRelease>>
RemoteRelease ==
    /\ remoteGrant /\ remoteGrant' = FALSE
    /\ UNCHANGED <<phase, grant, closed, owner, gen, snapshot, queued, leaf,
                    incomingRsp, badPublication, prematureRelease>>
Close ==
    /\ ~closed /\ ~grant /\ ~remoteGrant /\ closed' = TRUE
    /\ UNCHANGED <<phase, grant, remoteGrant, owner, gen, snapshot, queued,
                    leaf, incomingRsp, badPublication, prematureRelease>>
Next == CaptureClaim \/ Republish \/ Claim \/ RefuseClaim \/ CaptureCurrent
        \/ Prepare \/ Publish \/ CompleteSame \/ CompleteChanged \/ AckRsp
        \/ Release \/ Poison \/ RemoteCapture \/ RemoteRelease \/ Close
        \/ UNCHANGED vars
TypeOK == /\ phase \in {"Idle", "Claiming", "Claimed", "Architecture", "Publishing", "AwaitingRsp", "Committing", "Done", "Poisoned"}
          /\ owner \in {"Local", "Direct", "Running"} /\ gen \in 1..MaxGeneration
          /\ snapshot \in 1..MaxGeneration /\ grant \in BOOLEAN
          /\ remoteGrant \in BOOLEAN /\ closed \in BOOLEAN /\ queued \in BOOLEAN
          /\ leaf \in BOOLEAN /\ incomingRsp \in BOOLEAN
          /\ badPublication \in BOOLEAN /\ prematureRelease \in BOOLEAN
CloseExcludesAllGrants == closed => ~(grant \/ remoteGrant)
CpuPhasesRetainGrant == phase \in {"Claiming", "Claimed", "Architecture", "Publishing", "AwaitingRsp", "Committing", "Poisoned"} => grant
MembershipMatchesOwner == queued = (owner = "Local")
LeafMatchesClaim == leaf = (phase = "Claiming")
PublicationHasExactCurrency == ~badPublication
NoReleaseBeforeRsp == ~prematureRelease
Spec == Init /\ [][Next]_vars
===============================================================================
