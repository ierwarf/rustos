-------------------- MODULE JobSpawnPlacement --------------------
EXTENDS Naturals

(***************************************************************************
Bounded loader placement projection: request sender authentication and live
service-role admission are supplied by LoaderRequestAuthority; descriptor
transfer/copyout by IpcTransferAuthority. This model checks their job-selection
seam, not user-copy execution or production spinlocks. Requester and loader
have different immutable jobs. Explicit placement requires a typed descriptor
with CREATE_PROCESS; exec can never accept that placement. Captured identity
must survive kill/recycle checks at reserve and publication.
***************************************************************************)
RequesterJob == 1
LoaderJob == 2
VARIABLES phase, explicit, typed, create, exec, selected, captured,
          generation, live, publicationGeneration, supervisor, remaining
vars == <<phase, explicit, typed, create, exec, selected, captured,
          generation, live, publicationGeneration, supervisor, remaining>>

Init ==
    /\ phase = "start"
    /\ explicit \in BOOLEAN
    /\ typed \in BOOLEAN
    /\ create \in BOOLEAN
    /\ exec \in BOOLEAN
    /\ selected = 0
    /\ captured = 0
    /\ generation = 1
    /\ live = TRUE
    /\ publicationGeneration = 0
    /\ supervisor = TRUE
    /\ remaining = 0

Prepare ==
    /\ phase = "start"
    /\ (~explicit \/ (typed /\ create)) \* typed CREATE_PROCESS admission
    /\ ~(exec /\ explicit) \* exec cannot retarget
    /\ live
    /\ selected' = IF explicit THEN LoaderJob ELSE RequesterJob
    /\ captured' = generation
    /\ phase' = "prepared"
    /\ UNCHANGED <<explicit, typed, create, exec, generation, live, publicationGeneration, supervisor, remaining>>

Reserve ==
    /\ phase = "prepared"
    /\ live /\ captured = generation
    /\ phase' = "reserved"
    /\ UNCHANGED <<explicit, typed, create, exec, selected, captured, generation, live, publicationGeneration, supervisor, remaining>>

Publish ==
    /\ phase = "reserved"
    /\ live /\ captured = generation \* birth revalidation
    /\ publicationGeneration' = generation
    /\ phase' = "published"
    /\ UNCHANGED <<explicit, typed, create, exec, selected, captured, generation, live, supervisor, remaining>>

Kill ==
    /\ phase \in {"prepared", "reserved"}
    /\ live
    /\ live' = FALSE
    /\ UNCHANGED <<phase, explicit, typed, create, exec, selected, captured, generation, publicationGeneration, supervisor, remaining>>

Recycle ==
    /\ phase \in {"prepared", "reserved"}
    /\ ~live /\ generation = 1
    /\ generation' = 2
    /\ live' = TRUE
    /\ UNCHANGED <<phase, explicit, typed, create, exec, selected, captured, publicationGeneration, supervisor, remaining>>

\* The supervisor may see a failed/lost reply even if an unseen child was
\* published. A bounded kill must finish the attempt before authority is closed.
Retire(count) ==
    /\ phase \in {"prepared", "reserved", "published"}
    /\ phase' = "retiring"
    /\ remaining' = count
    /\ UNCHANGED <<explicit, typed, create, exec, selected, captured, generation, live, publicationGeneration, supervisor>>

Drain ==
    /\ phase = "retiring" /\ remaining > 0
    /\ remaining' = remaining - 1
    /\ supervisor' = supervisor \* pending kill retains custody
    /\ UNCHANGED <<phase, explicit, typed, create, exec, selected, captured, generation, live, publicationGeneration>>

Close ==
    /\ phase = "retiring"
    /\ remaining = 0 \* close only when empty
    /\ phase' = "closed"
    /\ supervisor' = FALSE
    /\ UNCHANGED <<explicit, typed, create, exec, selected, captured, generation, live, publicationGeneration, remaining>>

Next == Prepare \/ Reserve \/ Publish \/ Kill \/ Recycle
        \/ (\E count \in 0..2: Retire(count)) \/ Drain \/ Close \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {"start", "prepared", "reserved", "published", "retiring", "closed"}
    /\ explicit \in BOOLEAN /\ typed \in BOOLEAN /\ create \in BOOLEAN /\ exec \in BOOLEAN
    /\ selected \in 0..2 /\ captured \in 0..2 /\ generation \in 1..2
    /\ live \in BOOLEAN /\ publicationGeneration \in 0..2
    /\ supervisor \in BOOLEAN /\ remaining \in 0..2
PreparedIsAuthorized == phase # "start" => (~explicit \/ (typed /\ create))
ExecKeepsMembership == phase # "start" /\ exec => ~explicit /\ selected = RequesterJob
DefaultInheritsRequester == phase # "start" /\ ~explicit => selected = RequesterJob
ExplicitUsesAuthorizedJob == phase # "start" /\ explicit => selected = LoaderJob
BirthHasExactLiveJob == phase = "published" => live /\ captured = publicationGeneration
NoAbandonedMembers == remaining > 0 => supervisor
ClosedOnlyWhenEmpty == phase = "closed" => remaining = 0
=============================================================================
