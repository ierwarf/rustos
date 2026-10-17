-------------------- MODULE SchedulerLocalAuthority --------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
A bounded authority/refinement slice for kernel-ps local_dispatch::choose_local_next,
and the final claim in Scheduler::dispatch_schedule.
Two or three fair candidates on one CPU expose class/vruntime/slot preference
and partial-scan rejection. This is NOT a model of the full picker, locality
lag, min-granularity, full IPC/activation handoff, catalog admission,
saved-RSP validation, or multi-writer
seqlocks. One Local-to-Local generation republish and one Local-to-Remote
owner race can occur before or after selection. Only the exact validated
generation may be claimed; this does not model arbitrary concurrent writers.
A failed generation-bound final claim now uses the existing current/idle
fallback rather than dispatching the stale candidate. Catalog/exact-handoff
claims still assert on unexpected failure. One lock-free same-generation
runnable update may race the generation-bound owner CAS without publishing a
new queue epoch; the failed claim must leave queue membership unchanged. The
model does not refine the fallback's own owner claim, simultaneous writers,
or budget
changes other than the separately tested due-refill-only preparation.
An incomplete/transitioning affinity fixture refuses local authority; stable
negative fixtures omit a lifecycle-ineligible, CPU-excluded, or exact exhausted
context/domain peer and let the qualified current task claim. A stale stamp or
changed donation token instead stays incomplete. These abstract the mask
intersection and budget status at one timestamp; they do not model concurrent
affinity/donor writers or refills becoming due after the snapshot.
Voluntary yield excludes current and a due User reservation preempts an
otherwise eligible System current only for the bounded complete stable scan.
An invalid due User or overdue-System observation retries bounded fresh
queue/class epochs, then retains exact current/idle custody without catalog
peer authority or falling through to later policy arms. An incomplete *ordinary fair* observation instead
retries CPU-local queue/class epochs (bounded by four in production), retaining
a qualified current generation or exact idle fallback when retries exhaust;
this model abstracts the retry count as one fallback transition.
A bounded overdue-System fixture assumes one eligible ready-aged System peer
which must preempt an otherwise fair-favored current, while its incomplete
fixture defers to guarded current/idle custody. One empty-fair fixture assumes a stable
empty fair bitmap and an exact Local idle owner generation; a distinct invalid-idle fixture retains guarded
idle/current fallback without granting catalog fair authority. One borrowed-valid fixture assumes an exact donor identity/charge
stamp and unchanged live reply token; the existing borrowed fixture represents a missing
or invalid donor publication and must still reject local authority. Neither
models arbitrary reply-token writers nor donor lifetime races. A bounded DirectHandoff FIFO slice also checks selection and final claim
against the record's exact owner generation; this does not model catalog
admission, saved-frame races, FIFO concurrency, CPU migration, or activation.
Consumed FIFO IPC now refuses incomplete or non-Local publication without
catalog admission (the matching Direct generation materializes to Local);
incomplete non-FIFO activation/hint selection still has catalog gates. The
bounded Local-record slice binds a committed activation, ordinary spawn, or
consumed Generic FIFO identity and a Local owner snapshot to its final claim.
Generic records carry no owner generation while queued: a newer Local owner
may be captured at selection, never after it. The model omits the FIFO's
physical synchronization, saved frame, budgets and retirement.
Retained-current fixtures separately bind the freshly saved Local frame to
exact Current CPU custody and complete published admission. Missing lifecycle,
exhausted budget, released CPU custody or a republished generation cannot run.
One Local republish between selection and claim must lose the exact claim.
These abstract stable admission, not publication interleavings, actual stack
bytes, idle progress, interval settlement, teardown or global-guard removal.
***************************************************************************)

Current == "current"
Peer == "peer"
Third == "third"
IdleTask == "idle"
NoTask == "none"
Scenarios == {
    "fair-peer", "fair-current", "class-peer", "class-current",
    "three-class-third", "three-runtime-third", "three-tie-peer",
    "borrowed", "borrowed-valid", "budget-refused", "frame-refused", "lifecycle-refused",
    "affinity-refused", "stable-affinity-skipped", "stable-lifecycle-skipped",
    "stable-budget-skipped",
    "empty-fair", "empty-idle-invalid", "overdue-system", "overdue-invalid",
    "yield", "reservation", "reservation-invalid", "ipc", "activation", "catalog-recheck",
    "queue-race", "class-race", "owner-race", "claim-race", "local-aba",
    "metadata-race", "direct-live", "direct-stale", "direct-claim-race",
    "activation-live", "activation-stale", "activation-reuse", "activation-claim-race",
    "spawn-live", "spawn-stale", "spawn-reuse", "spawn-claim-race",
    "generic-live", "generic-republished-before-selection", "generic-reuse", "generic-claim-race",
    "current-live", "current-stale", "current-cpu-lost", "current-budget-empty",
    "current-lifecycle-unavailable", "current-claim-race"
}
DirectModes == {"direct-live", "direct-stale", "direct-claim-race"}
ActivationModes == {"activation-live", "activation-stale", "activation-reuse", "activation-claim-race"}
SpawnModes == {"spawn-live", "spawn-stale", "spawn-reuse", "spawn-claim-race"}
GenericModes == {"generic-live", "generic-republished-before-selection", "generic-reuse", "generic-claim-race"}
BoundLocalModes == ActivationModes \cup SpawnModes \cup GenericModes
RetainedCurrentModes == {"current-live", "current-stale", "current-cpu-lost", "current-budget-empty", "current-lifecycle-unavailable", "current-claim-race"}
RetainedCurrentRefusedModes == {"current-stale", "current-cpu-lost", "current-budget-empty", "current-lifecycle-unavailable"}
ExactModes == DirectModes \cup BoundLocalModes \cup RetainedCurrentModes

VARIABLES mode, phase, queueEpoch, classEpoch, seenQueue, seenClass,
          owner, ownerGen, frameGen, runnable, decision, selected, tokenGen
vars == <<mode, phase, queueEpoch, classEpoch, seenQueue, seenClass,
          owner, ownerGen, frameGen, runnable, decision, selected, tokenGen>>

RetainedCurrentPublishedAdmission == mode \notin {"current-cpu-lost", "current-budget-empty", "current-lifecycle-unavailable"}

HasThird(m) == m \in {"three-class-third", "three-runtime-third", "three-tie-peer"}
ClassOf(t, m) ==
    IF t = Current /\ m \in {"class-current", "reservation", "reservation-invalid", "overdue-system"} THEN "system"
    ELSE IF t = Peer /\ m \in {"class-peer", "overdue-system"} THEN "system"
    ELSE IF t = Third /\ m = "three-class-third" THEN "system"
    ELSE "user"
RuntimeOf(t, m) ==
    IF t = Current THEN
        IF m \in {"fair-current", "class-peer", "overdue-system"} THEN 0 ELSE 2
    ELSE IF t = Peer THEN
        IF m \in {"fair-current", "class-peer", "three-runtime-third", "overdue-system"}
            THEN 2 ELSE 0
    ELSE IF m = "three-class-third" THEN 99
    ELSE IF m = "three-tie-peer" THEN 0
    ELSE 0
SlotOrder(t) == IF t = Current THEN 0 ELSE IF t = Peer THEN 1 ELSE 2
FairKey(t, m) == RuntimeOf(t, m) + (IF t = Current THEN 1 ELSE 0)
Better(a, b, m) ==
    IF ClassOf(a, m) # ClassOf(b, m) THEN ClassOf(a, m) = "system"
    ELSE IF FairKey(a, m) # FairKey(b, m) THEN FairKey(a, m) < FairKey(b, m)
    ELSE SlotOrder(a) < SlotOrder(b)
StableNegativePeer(m) == m \in {"stable-affinity-skipped", "stable-lifecycle-skipped", "stable-budget-skipped"}
LocalFairPick(m) ==
    LET first == IF StableNegativePeer(m) THEN Current
                 ELSE IF Better(Peer, Current, m) THEN Peer ELSE Current
    IN IF HasThird(m) /\ Better(Third, first, m) THEN Third ELSE first
LocalPolicyPick(m) ==
    IF m = "reservation" THEN Peer
    ELSE IF m = "empty-fair" THEN IdleTask
    ELSE IF m = "overdue-system" THEN Peer
    ELSE LocalFairPick(m)

\* Independent scenario expectations, NOT a catalog code oracle.
ExpectedFairPick(m) ==
    IF m = "empty-fair" THEN IdleTask
    ELSE IF m \in {"three-class-third", "three-runtime-third"} THEN Third
    ELSE IF m \in {"fair-current", "class-current", "stable-affinity-skipped", "stable-lifecycle-skipped", "stable-budget-skipped"} THEN Current ELSE Peer

Excluded(m) == m \in {"ipc", "activation"}
Incomplete(m) == m \in {
    "borrowed", "budget-refused", "frame-refused", "lifecycle-refused",
    "empty-idle-invalid", "overdue-invalid", "reservation-invalid",
    "affinity-refused"
}

Qualified ==
    /\ ~Excluded(mode)
    /\ mode \notin ExactModes
    /\ ~Incomplete(mode)
    /\ mode # "catalog-recheck"
    /\ queueEpoch = seenQueue
    /\ classEpoch = seenClass
    /\ owner = "local"
    /\ ownerGen = frameGen
    /\ runnable

Init ==
    /\ mode \in Scenarios
    /\ phase = "start"
    /\ queueEpoch = 0
    /\ classEpoch = 0
    /\ seenQueue = 0
    /\ seenClass = 0
    /\ owner = IF mode \in DirectModes THEN "direct" ELSE "local"
    /\ ownerGen = IF mode \in {"direct-stale", "activation-stale", "spawn-stale", "generic-republished-before-selection", "current-stale"} THEN 2 ELSE 1
    /\ frameGen = 1
    /\ tokenGen = 1
    /\ runnable = TRUE
    /\ decision = "undecided"
    /\ selected = NoTask

Observe ==
    /\ phase = "start"
    /\ phase' = "observed"
    /\ seenQueue' = queueEpoch
    /\ seenClass' = classEpoch
    /\ UNCHANGED <<mode, queueEpoch, classEpoch, owner, ownerGen,
                    frameGen, runnable, decision, selected, tokenGen>>

ChangeQueue ==
    /\ mode = "queue-race"
    /\ phase = "observed"
    /\ queueEpoch = 0
    /\ queueEpoch' = 2
    /\ UNCHANGED <<mode, phase, classEpoch, seenQueue, seenClass,
                    owner, ownerGen, frameGen, runnable, decision, selected, tokenGen>>

ChangeClass ==
    /\ mode = "class-race"
    /\ phase = "observed"
    /\ classEpoch = 0
    /\ classEpoch' = 2
    /\ UNCHANGED <<mode, phase, queueEpoch, seenQueue, seenClass,
                    owner, ownerGen, frameGen, runnable, decision, selected, tokenGen>>

ChangeOwner ==
    /\ (mode = "owner-race" /\ phase = "observed")
       \/ (mode \in {"claim-race", "local-aba", "direct-claim-race", "activation-claim-race", "spawn-claim-race", "generic-claim-race", "current-claim-race"} /\ phase = "chosen")
    /\ owner = IF mode = "direct-claim-race" THEN "direct" ELSE "local"
    /\ ownerGen = 1
    /\ owner' = IF mode \in {"local-aba", "direct-claim-race", "activation-claim-race", "spawn-claim-race", "generic-claim-race", "current-claim-race"} THEN owner ELSE "remote"
    /\ ownerGen' = 2
    /\ UNCHANGED <<mode, phase, queueEpoch, classEpoch, seenQueue,
                    seenClass, frameGen, runnable, decision, selected, tokenGen>>

ChangeMetadata ==
    /\ mode = "metadata-race"
    /\ phase = "chosen"
    /\ runnable
    /\ runnable' = FALSE
    /\ UNCHANGED <<mode, phase, queueEpoch, classEpoch, seenQueue,
                    seenClass, owner, ownerGen, frameGen, decision, selected, tokenGen>>

\* Consuming a direct FIFO record requires the exact published owner.
\* The catalog admission and frame check precede this bounded abstraction.
DecideDirect ==
    /\ mode \in DirectModes /\ phase = "observed"
    /\ LET eligible == owner = "direct" /\ ownerGen = tokenGen /\ runnable
       IN /\ phase' = IF eligible THEN "chosen" ELSE "done"
          /\ decision' = IF eligible THEN "handoff" ELSE "discarded"
          /\ selected' = IF eligible THEN Peer ELSE NoTask
    /\ UNCHANGED <<mode, queueEpoch, classEpoch, seenQueue, seenClass,
                    owner, ownerGen, frameGen, runnable, tokenGen>>

\* Model the production bound claim, but not catalog admission or arbitrary
\* publication races. A republish after consumption must not run that peer.
ClaimDirect ==
    /\ mode \in DirectModes /\ phase = "chosen"
    /\ LET valid == owner = "direct" /\ ownerGen = tokenGen /\ runnable
       IN /\ phase' = "done"
          /\ decision' = IF valid THEN "direct-committed" ELSE "fallback"
          /\ owner' = IF valid THEN "running" ELSE owner
          /\ selected' = IF valid THEN selected ELSE NoTask
    /\ UNCHANGED <<mode, queueEpoch, classEpoch, seenQueue, seenClass,
                    ownerGen, frameGen, runnable, tokenGen>>

\* An activation/spawn record captures its generation before selection; a
\* Generic record remains unbound in the FIFO and captures Local custody only
\* on consumption. Stale identities in either path cannot gain authority.
DecideActivation ==
    /\ mode \in BoundLocalModes /\ phase = "observed"
    /\ LET generation == IF mode \in GenericModes THEN ownerGen ELSE tokenGen
           eligible == mode \notin {"activation-reuse", "spawn-reuse", "generic-reuse"}
                        /\ owner = "local" /\ ownerGen = generation /\ runnable
       IN /\ phase' = IF eligible THEN "chosen" ELSE "done"
          /\ decision' = IF eligible THEN "activation" ELSE "discarded"
          /\ selected' = IF eligible THEN Peer ELSE NoTask
          /\ tokenGen' = generation
    /\ UNCHANGED <<mode, queueEpoch, classEpoch, seenQueue, seenClass,
                    owner, ownerGen, frameGen, runnable>>

ClaimActivation ==
    /\ mode \in BoundLocalModes /\ phase = "chosen"
    /\ LET valid == owner = "local" /\ ownerGen = tokenGen /\ runnable
       IN /\ phase' = "done"
          /\ decision' = IF valid THEN "activation-committed" ELSE "fallback"
          /\ owner' = IF valid THEN "running" ELSE owner
          /\ selected' = IF valid THEN selected ELSE NoTask
    /\ UNCHANGED <<mode, queueEpoch, classEpoch, seenQueue, seenClass,
                    ownerGen, frameGen, runnable, tokenGen>>

DecideRetainedCurrent ==
    /\ mode \in RetainedCurrentModes /\ phase = "observed"
    /\ LET eligible == RetainedCurrentPublishedAdmission
                       /\ owner = "local" /\ ownerGen = frameGen /\ runnable
       IN /\ phase' = IF eligible THEN "chosen" ELSE "done"
          /\ decision' = IF eligible THEN "current-selected" ELSE "current-refused"
          /\ selected' = IF eligible THEN Current ELSE NoTask
          /\ tokenGen' = frameGen
    /\ UNCHANGED <<mode, queueEpoch, classEpoch, seenQueue, seenClass,
                    owner, ownerGen, frameGen, runnable>>

ClaimRetainedCurrent ==
    /\ mode \in RetainedCurrentModes /\ phase = "chosen"
    /\ LET valid == RetainedCurrentPublishedAdmission /\ owner = "local"
                    /\ ownerGen = tokenGen /\ runnable
       IN /\ phase' = "done"
          /\ decision' = IF valid THEN "current-committed" ELSE "fallback"
          /\ owner' = IF valid THEN "running" ELSE owner
          /\ selected' = IF valid THEN Current ELSE NoTask
    /\ UNCHANGED <<mode, queueEpoch, classEpoch, seenQueue, seenClass,
                    ownerGen, frameGen, runnable, tokenGen>>

Decide ==
    /\ mode \notin ExactModes
    /\ phase = "observed"
    /\ phase' = IF Qualified THEN "chosen" ELSE "done"
    /\ decision' = IF Qualified THEN "local"
                   ELSE IF mode \in {"overdue-invalid", "reservation-invalid"}
                        THEN "guarded" ELSE "catalog"
    /\ selected' = IF Qualified THEN LocalPolicyPick(mode) ELSE NoTask
    /\ UNCHANGED <<mode, queueEpoch, classEpoch, seenQueue, seenClass,
                    owner, ownerGen, frameGen, runnable, tokenGen>>

\* The real scheduler also validates the saved context and prepares the due
\* budget refill. These are separate predicates in production, abstracted by
\* Incomplete. The local claim binds the validated owner generation. If it
\* fails, the scheduler takes its existing current/idle fallback, not a new
\* catalog fair scan. The fallback's separate claim is outside this model.
Claim ==
    /\ mode \notin ExactModes
    /\ phase = "chosen"
    /\ LET valid == owner = "local" /\ ownerGen = frameGen
                     /\ runnable
                     /\ queueEpoch = seenQueue /\ classEpoch = seenClass
       IN /\ phase' = "done"
          /\ decision' = IF valid THEN "committed" ELSE "fallback"
          /\ owner' = IF valid /\ selected # Current THEN "running" ELSE owner
          /\ selected' = IF valid THEN selected ELSE NoTask
    /\ UNCHANGED <<mode, queueEpoch, classEpoch, seenQueue, seenClass,
                    ownerGen, frameGen, runnable, tokenGen>>

Idle == phase = "done" /\ UNCHANGED vars
Next == Observe \/ ChangeQueue \/ ChangeClass \/ ChangeOwner
        \/ ChangeMetadata \/ DecideDirect \/ ClaimDirect
        \/ DecideActivation \/ ClaimActivation \/ DecideRetainedCurrent
        \/ ClaimRetainedCurrent \/ Decide \/ Claim \/ Idle
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ mode \in Scenarios
    /\ phase \in {"start", "observed", "chosen", "done"}
    /\ queueEpoch \in {0, 2} /\ classEpoch \in {0, 2}
    /\ seenQueue \in {0, 2} /\ seenClass \in {0, 2}
    /\ owner \in {"local", "remote", "direct", "running"}
    /\ ownerGen \in {1, 2} /\ frameGen = 1 /\ tokenGen \in {1, 2}
    /\ runnable \in BOOLEAN
    /\ decision \in {"undecided", "catalog", "guarded", "local", "committed", "fallback",
                    "handoff", "discarded", "direct-committed", "activation", "activation-committed",
                    "current-selected", "current-refused", "current-committed"}
    /\ selected \in {NoTask, Current, Peer, Third, IdleTask}

ExcludedArmsNeverAcquireLocalAuthority ==
    Excluded(mode) => decision \notin {"local", "committed"}

YieldWithEligiblePeerDoesNotRetainOutgoingSlot ==
    mode = "yield" /\ decision \in {"local", "committed"} => selected = Peer

ReservationPrefersUserBeforeOrdinarySystem ==
    mode = "reservation" /\ decision \in {"local", "committed"} =>
        ClassOf(selected, mode) = "user" /\ selected = Peer

IncompleteEarlyPolicyUsesGuardedCustody ==
    mode \in {"reservation-invalid", "overdue-invalid"} /\ phase = "done" =>
        decision = "guarded" /\ selected = NoTask

ForbiddenAffinityCannotClaimLocal ==
    mode = "affinity-refused" => decision \notin {"local", "committed"}

StableNegativePeerDoesNotBlockCurrent ==
    StableNegativePeer(mode) =>
        /\ (decision \in {"local", "committed"} => selected = Current)
        /\ (phase = "done" => decision = "committed")

ExactBudgetDenialSkipsOnlyPeer ==
    mode = "stable-budget-skipped" /\ decision \in {"local", "committed"} =>
        selected = Current

ValidatedBorrowedOwnerMayClaim ==
    mode = "borrowed-valid" /\ phase = "done" => decision = "committed"

EmptyFairClaimsOnlyExactIdle ==
    mode = "empty-fair" /\ decision \in {"local", "committed"} => selected = IdleTask

OverdueSystemMustPreemptFairCurrent ==
    mode = "overdue-system" /\ decision \in {"local", "committed"} => selected = Peer

IncompleteScanFallsBack ==
    Incomplete(mode) => decision \notin {"local", "committed"}

StableEpochsBeforeLocalDecision ==
    decision = "local" => queueEpoch = seenQueue /\ classEpoch = seenClass

FairPickMatchesIndependentScenarioExpectation ==
    decision \in {"local", "committed"} =>
        selected = ExpectedFairPick(mode)

ExactClaimOrSafeFallback ==
    decision = "committed" =>
        /\ phase = "done"
        /\ ownerGen = frameGen
        /\ runnable
        /\ queueEpoch = seenQueue /\ classEpoch = seenClass
        /\ (selected # Current => owner = "running")

CatalogHasNoLocalClaim ==
    decision = "catalog" => selected = NoTask

RejectedClaimReturnsToFallbackWithoutStaleDispatch ==
    decision = "fallback" => phase = "done" /\ selected = NoTask

FailedBoundClaimUsesFallback ==
    phase = "done" /\ mode \in {"claim-race", "local-aba", "metadata-race"}
    /\ (ownerGen # frameGen \/ ~runnable) => decision = "fallback"

DirectRecordClaimBindsConsumedGeneration ==
    mode \in DirectModes /\ decision = "direct-committed" =>
        /\ phase = "done" /\ owner = "running" /\ selected = Peer
        /\ ownerGen = tokenGen

DirectStaleRecordLosesUrgency ==
    mode = "direct-stale" /\ phase = "done" =>
        decision = "discarded" /\ selected = NoTask

DirectRepublishAfterSelectionCannotClaim ==
    mode = "direct-claim-race" /\ phase = "done" /\ ownerGen # tokenGen =>
        decision = "fallback" /\ selected = NoTask

DirectLiveRecordCanClaim ==
    mode = "direct-live" /\ phase = "done" => decision = "direct-committed"

ActivationClaimBindsConsumedGeneration ==
    decision = "activation-committed" =>
        /\ mode \in BoundLocalModes /\ phase = "done"
        /\ ownerGen = tokenGen /\ owner = "running" /\ selected = Peer

ActivationStaleOrReusedTokenCannotClaim ==
    mode \in {"activation-stale", "activation-reuse", "spawn-stale", "spawn-reuse", "generic-reuse"}
    /\ phase = "done" =>
        decision = "discarded" /\ selected = NoTask

ActivationRepublishAfterSelectionCannotClaim ==
    mode \in {"activation-claim-race", "spawn-claim-race", "generic-claim-race"}
    /\ phase = "done" /\ ownerGen # tokenGen =>
        decision = "fallback" /\ selected = NoTask

ActivationLiveTokenCanClaim ==
    mode \in {"activation-live", "spawn-live", "generic-live", "generic-republished-before-selection"} /\ phase = "done" =>
        decision = "activation-committed"

RetainedCurrentCommitHasExactFrameAndAdmission ==
    decision = "current-committed" =>
        /\ mode \in RetainedCurrentModes /\ mode \notin RetainedCurrentRefusedModes
        /\ ownerGen = tokenGen /\ tokenGen = frameGen /\ runnable
        /\ phase = "done" /\ owner = "running" /\ selected = Current

RetainedCurrentRefusalCannotGainAuthority ==
    mode \in RetainedCurrentRefusedModes =>
        decision \notin {"current-selected", "current-committed"}

RetainedCurrentLiveFrameCanRun ==
    mode = "current-live" /\ phase = "done" => decision = "current-committed"

==========================================================
