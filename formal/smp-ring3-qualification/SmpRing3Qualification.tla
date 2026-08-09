---------------------- MODULE SmpRing3Qualification -----------------------
EXTENDS Naturals, FiniteSets

(******************************************************************************
Owner: KVM-private Ring3 SMP qualification provenance and evidence chain.

The model deliberately separates two boundaries:

  * Ring0 binds a one-shot deferred child to the live SESSIOND owner identity
    (pid, process generation, mm generation, and endpoint epoch) and to the
    exact target process identity.  The generations are kernel-side state;
    they are not claimed to be host-visible log fields.
  * The host-visible self-framed evidence contains the kernel-selected
    nonzero binding_id, PID/TID, CPU, work, deadline, and emission time.  Its
    FNV framing is transport integrity only, not process authentication.

CreateDeferredChild, Bind, and Activate model the same authority transaction:
the private child stays suspended until an exact, one-shot binding exists.
Activate stamps the sole absolute deadline.  Only that still-live target may
move workers Ready -> Start -> Finish -> Joined and emit Complete.  SESSIOND
replacement, target exit, exec/mm replacement, PID reuse, or expiry removes
active authority and leaves no later phase transition enabled.

Finite clock values model the ordering of an activation-stamped deadline, not
wall-clock throughput.  Fairness is only the admitted, still-active local
worker progression; it does not promise scheduler rate or host observation of
kernel generations.  UI/display readiness and production DVM-volume readiness
are independent external observations.  A volume probe that is transiently
unavailable leaves only the optional private reconciliation pending; the
ordinary signed catalog may commit independently.  Only a ready-volume probe
of the exact private contract may admit the private child, and that child must
be the digest-verified `apps/smpqual/smpqual.elf` from the signed early-system
allowlist rather than same-path DVM bytes.  A ready-volume true-absence probe
may settle the optional path as absent.
*******************************************************************************)

CONSTANTS MaxWorkers, SupportedCounts, QualifiedWork, QualifiedDeadline

Ready == "ready"
Start == "start"
Finish == "finish"
Complete == "complete"
WorkerPhases == {Ready, Start, Finish}
Phases == WorkerPhases \cup {Complete}

Pending == "pending"
Successful == "successful"
TimedOut == "timed-out"
RevokedResult == "revoked"
Results == {Pending, Successful, TimedOut, RevokedResult}

NotCreated == "not-created"
Deferred == "deferred"
BoundSuspended == "bound-suspended"
Active == "active"
Revoked == "revoked"
Terminal == "terminal"
BindingStates == {NotCreated, Deferred, BoundSuspended, Active, Revoked, Terminal}

WorkerInit == "init"
WorkerReady == "worker-ready"
WorkerStarted == "worker-started"
WorkerFinished == "worker-finished"
WorkerJoined == "worker-joined"
WorkerStates == {WorkerInit, WorkerReady, WorkerStarted, WorkerFinished, WorkerJoined}

\* All identities are numeric records so TLC can enumerate exact stale and
\* reused variants without mistaking a string sentinel for a record.
NoIdentity == [pid |-> 0, pgen |-> 0, mmgen |-> 0]
QualificationIdentity == [pid |-> 1, pgen |-> 1, mmgen |-> 1]
TargetExecIdentity == [pid |-> 1, pgen |-> 1, mmgen |-> 2]
TargetReusedPidIdentity == [pid |-> 1, pgen |-> 2, mmgen |-> 2]
IdentityValues == {NoIdentity, QualificationIdentity, TargetExecIdentity,
                   TargetReusedPidIdentity}

SessiondIdentity == [pid |-> 10, pgen |-> 1, mmgen |-> 1]
SessiondReusedIdentity == [pid |-> 10, pgen |-> 2, mmgen |-> 2]
NoEndpoint == [owner_pid |-> 0, epoch |-> 0]
SessiondEndpoint == [owner_pid |-> 10, epoch |-> 1]
StaleSessiondEndpoint == [owner_pid |-> 10, epoch |-> 2]
EndpointValues == {NoEndpoint, SessiondEndpoint, StaleSessiondEndpoint}

NoOwnerAuthority == [identity |-> NoIdentity, endpoint |-> NoEndpoint]
SessiondAuthority == [identity |-> SessiondIdentity,
                      endpoint |-> SessiondEndpoint]
StaleSessiondEpochAuthority == [identity |-> SessiondIdentity,
                                 endpoint |-> StaleSessiondEndpoint]
ReusedSessiondAuthority == [identity |-> SessiondReusedIdentity,
                             endpoint |-> StaleSessiondEndpoint]
OwnerAuthorityValues == {NoOwnerAuthority, SessiondAuthority,
                         StaleSessiondEpochAuthority, ReusedSessiondAuthority}
OwnerCandidates == {SessiondAuthority, StaleSessiondEpochAuthority,
                    ReusedSessiondAuthority}
TargetCandidates == {QualificationIdentity, TargetReusedPidIdentity}
TargetInvalidations == {NoIdentity, TargetExecIdentity, TargetReusedPidIdentity}

NoBindingId == 0
KernelBindingId == 1
ForeignBindingId == 2
BindingIdValues == {NoBindingId, KernelBindingId, ForeignBindingId}

ExactPrivateContract == "exact-private-contract"
TrueAbsentPrivateContract == "true-absent-private-contract"
PrivateContractTruths == {ExactPrivateContract, TrueAbsentPrivateContract}

PrivateQualificationPending == "private-qualification-pending"
PrivateExactContractVisible == "private-exact-contract-visible"
PrivateContractAbsent == "private-contract-absent"
PrivateQualificationStates == {PrivateQualificationPending,
                               PrivateExactContractVisible,
                               PrivateContractAbsent}

NoQualificationExecutable ==
    [path |-> "", provenance |-> "none"]
TrustedEarlySystemQualificationExecutable ==
    [path |-> "apps/smpqual/smpqual.elf",
     provenance |-> "digest-verified-early-system"]
DvmSubstitutedQualificationExecutable ==
    [path |-> "apps/smpqual/smpqual.elf",
     provenance |-> "dvm-substituted"]
QualificationExecutableCandidates ==
    {NoQualificationExecutable, TrustedEarlySystemQualificationExecutable,
     DvmSubstitutedQualificationExecutable}

CatalogUnobserved == "catalog-unobserved"
CatalogVolumeTransientlyUnavailable == "catalog-volume-transiently-unavailable"
CatalogExactContractObserved == "catalog-exact-contract-observed"
CatalogTrueAbsenceObserved == "catalog-true-absence-observed"
CatalogObservations == {CatalogUnobserved, CatalogVolumeTransientlyUnavailable,
                        CatalogExactContractObserved, CatalogTrueAbsenceObserved}
\* Output is carried as one compact, acceptance-only record alongside the
\* catalog transaction state.  It is independent of catalog policy: catalog
\* transitions preserve it, while the five output-loss actions below are the
\* only writers.  Counters retain the source-visible zero/nonzero distinction
\* without expanding each host evidence record or each worker's transport
\* state.
QualificationOutputRecords ==
    [criticalDropped : BOOLEAN,
     qualificationMilestonesDropped : 0..1,
     qualificationDebugBytesDiscarded : 0..1,
     schedulerMeasurementDropped : BOOLEAN]
CleanQualificationOutput ==
    [criticalDropped |-> FALSE,
     qualificationMilestonesDropped |-> 0,
     qualificationDebugBytesDiscarded |-> 0,
     schedulerMeasurementDropped |-> FALSE]
CatalogRecords ==
    [volumeReady : BOOLEAN,
     contractTruth : PrivateContractTruths,
     ordinaryCommitted : BOOLEAN,
     privateState : PrivateQualificationStates,
     observation : CatalogObservations,
     executable : QualificationExecutableCandidates,
     dvmSubstitutionAttempted : BOOLEAN,
     output : QualificationOutputRecords]

VARIABLES workerCount, evidence, completed, workerPhase,
          bindingState, liveOwnerAuthority, liveTargetIdentity,
          deferredOwnerAuthority, deferredTargetIdentity,
          boundOwnerAuthority, boundTargetIdentity, bindingId,
          activationCount, activationTick, absoluteDeadline, now,
          uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted, result

vars == <<workerCount, evidence, completed, workerPhase,
          bindingState, liveOwnerAuthority, liveTargetIdentity,
          deferredOwnerAuthority, deferredTargetIdentity,
          boundOwnerAuthority, boundTargetIdentity, bindingId,
          activationCount, activationTick, absoluteDeadline, now,
          uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

NoDeadline == 0
PreActivationTicks == 0..1
\* The extra slack values make a corrupt activation stamp well typed, so TLC
\* rejects it by the semantic invariant rather than a type error.
AbsoluteDeadlineValues ==
    {NoDeadline} \cup
    {tick + QualifiedDeadline + slack :
        tick \in PreActivationTicks, slack \in 0..2}
TimeValues == PreActivationTicks \cup AbsoluteDeadlineValues

\* Preserve the existing host-visible worker/CPU/TID/work universe.  Process
\* and mm generations intentionally do not appear here: they are admitted by
\* the kernel Binding state, while the parser sees only the opaque binding id.
NoCpu == MaxWorkers
ForeignWorker == MaxWorkers + 1
QualificationPid == 1
ForeignPid == 2
ForeignTid == MaxWorkers + 1
WorkerIds == 0..(MaxWorkers - 1)
MaxWorkUnits == 10000000
RecordWorkValues == {0, QualifiedWork, QualifiedWork + 1}

RecordValues ==
    [worker : WorkerIds \cup {ForeignWorker},
     cpu : WorkerIds \cup {NoCpu},
     pid : {QualificationPid, ForeignPid},
     tid : (1..MaxWorkers) \cup {ForeignTid},
     work : RecordWorkValues,
     at : TimeValues,
     deadline : AbsoluteDeadlineValues,
     binding_id : BindingIdValues]

RecordFor(worker, cpu, pid, tid, work, at, deadline, binding_id) ==
    [worker |-> worker,
     cpu |-> cpu,
     pid |-> pid,
     tid |-> tid,
     work |-> work,
     at |-> at,
     deadline |-> deadline,
     binding_id |-> binding_id]

NoRecord ==
    RecordFor(ForeignWorker, NoCpu, ForeignPid, ForeignTid, 0, NoDeadline,
              NoDeadline, NoBindingId)

CorrectRecord(worker) ==
    RecordFor(worker, worker, QualificationPid, worker + 1, QualifiedWork,
              now, absoluteDeadline, bindingId)

ForeignRecord(slot) ==
    RecordFor(ForeignWorker, NoCpu, ForeignPid, ForeignTid, QualifiedWork,
              slot, QualifiedDeadline, ForeignBindingId)

ExpectedWorkers == 0..(workerCount - 1)

output == catalog.output
QualificationOutputClean ==
    /\ ~output.criticalDropped
    /\ output.qualificationMilestonesDropped = 0
    /\ output.qualificationDebugBytesDiscarded = 0

Recorded(phase, worker) == evidence[phase][worker] # NoRecord
ReadyWorkers == {worker \in ExpectedWorkers : Recorded(Ready, worker)}
StartedWorkers == {worker \in ExpectedWorkers : Recorded(Start, worker)}
FinishedWorkers == {worker \in ExpectedWorkers : Recorded(Finish, worker)}
JoinedWorkers == {worker \in ExpectedWorkers : workerPhase[worker] = WorkerJoined}
CompletedWorkers == completed
AllReady == ReadyWorkers = ExpectedWorkers
AllWorkCompleted == CompletedWorkers = ExpectedWorkers
AllFinished == FinishedWorkers = ExpectedWorkers
AllJoined == JoinedWorkers = ExpectedWorkers
TerminalRecorded == Recorded(Complete, 0)
QualificationEvidenceEventCount ==
    Cardinality(ReadyWorkers) + Cardinality(StartedWorkers) +
    Cardinality(FinishedWorkers) + (IF TerminalRecorded THEN 1 ELSE 0)
AcceptedRecords ==
    {evidence[phase][worker] : phase \in Phases, worker \in WorkerIds} \ {NoRecord}
AcceptedBindingIds == {record.binding_id : record \in AcceptedRecords}

\* TLC explores one canonical permutation of otherwise independent worker
\* transitions.  Every requested worker still emits every phase and retains
\* its own CPU/TID fields; the model does not claim this is a runtime schedule.
NextReadyWorker == Cardinality(ReadyWorkers)
NextStartedWorker == Cardinality(StartedWorkers)
NextCompletedWorker == Cardinality(CompletedWorkers)
NextFinishedWorker == Cardinality(FinishedWorkers)
NextJoinedWorker == Cardinality(JoinedWorkers)

ExactRecordFor(worker, record) ==
    /\ record \in RecordValues
    /\ record.worker = worker
    /\ record.cpu = worker
    /\ record.pid = QualificationPid
    /\ record.tid = worker + 1
    /\ record.work = QualifiedWork
    /\ record.work \in 1..MaxWorkUnits
    /\ record.deadline = absoluteDeadline

BindingIsLiveExact ==
    /\ bindingState = BoundSuspended
    /\ bindingId = KernelBindingId
    /\ boundOwnerAuthority = SessiondAuthority
    /\ boundTargetIdentity = QualificationIdentity
    /\ boundOwnerAuthority = liveOwnerAuthority
    /\ boundTargetIdentity = liveTargetIdentity

ActiveRecordAdmission(worker) ==
    /\ result = Pending
    /\ bindingState = Active
    /\ worker \in ExpectedWorkers
    /\ bindingId = KernelBindingId
    /\ liveOwnerAuthority = boundOwnerAuthority
    /\ liveTargetIdentity = boundTargetIdentity
    /\ now < absoluteDeadline

ProductionVolumeReady == catalog.volumeReady

ExactPrivateContractVisible ==
    /\ catalog.contractTruth = ExactPrivateContract
    /\ catalog.privateState = PrivateExactContractVisible
    /\ catalog.observation = CatalogExactContractObserved

TrustedQualificationExecutableSelected ==
    catalog.executable = TrustedEarlySystemQualificationExecutable

\* This is deliberately a state predicate rather than a UI policy branch.
\* The exact private contract must be visible through the production DVM
\* volume, but UI/display readiness never authorizes or blocks the same
\* catalog-to-deferred-child transaction.
PrivateDeferredChildAdmissionEnabled ==
    /\ result = Pending
    /\ bindingState = NotCreated
    /\ liveOwnerAuthority = SessiondAuthority
    /\ liveTargetIdentity = QualificationIdentity
    /\ ProductionVolumeReady
    /\ ExactPrivateContractVisible
    /\ TrustedQualificationExecutableSelected

FinishAdmission(worker) ==
    /\ ActiveRecordAdmission(worker)
    /\ AllWorkCompleted
    /\ workerPhase[worker] = WorkerStarted

\* Every qualification phase has the same first-loss acceptance effect.  The
\* phase-specific actions retain their normal logical evidence/FSM update, but
\* converge on this one compact output state; later critical drops are not
\* separately modeled because the first already makes acceptance impossible.
\* The pre-existing worker order is canonical, so the symmetric Ready/Start/
\* Finish drop actions use worker zero as the representative critical frame:
\* a loss at any worker has identical host-acceptance counters and outcome.
CriticalOutputDropAdmission(worker) ==
    /\ ActiveRecordAdmission(worker)
    /\ ~output.criticalDropped

Init ==
    /\ MaxWorkers = 8
    /\ SupportedCounts = {1, 2, 4, 8}
    /\ QualifiedWork \in 1..MaxWorkUnits
    /\ QualifiedDeadline \in 100..30000
    /\ workerCount \in SupportedCounts
    /\ evidence = [phase \in Phases |-> [worker \in WorkerIds |-> NoRecord]]
    /\ completed = {}
    /\ workerPhase = [worker \in WorkerIds |-> WorkerInit]
    /\ bindingState = NotCreated
    /\ liveOwnerAuthority = SessiondAuthority
    /\ liveTargetIdentity = QualificationIdentity
    /\ deferredOwnerAuthority = NoOwnerAuthority
    /\ deferredTargetIdentity = NoIdentity
    /\ boundOwnerAuthority = NoOwnerAuthority
    /\ boundTargetIdentity = NoIdentity
    /\ bindingId = NoBindingId
    /\ activationCount = 0
    /\ activationTick = NoDeadline
    /\ absoluteDeadline = NoDeadline
    /\ now = 0
    /\ uiReady = FALSE
    /\ catalog \in [volumeReady : BOOLEAN,
                    contractTruth : PrivateContractTruths,
                    ordinaryCommitted : BOOLEAN,
                    privateState : {PrivateQualificationPending},
                    observation : {CatalogUnobserved},
                    executable : {NoQualificationExecutable},
                    dvmSubstitutionAttempted : {FALSE},
                    output : {CleanQualificationOutput}]
    /\ duplicateEvidenceAccepted = FALSE
    /\ expiredEvidenceAccepted = FALSE
    /\ result = Pending

CreateDeferredChild ==
    /\ PrivateDeferredChildAdmissionEnabled
    /\ bindingState' = Deferred
    /\ deferredOwnerAuthority' = liveOwnerAuthority
    /\ deferredTargetIdentity' = liveTargetIdentity
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    liveOwnerAuthority, liveTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline, now,
                    uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

\* The display service can become ready or unavailable independently of the
\* private scheduler qualification.  This action models that environment
\* transition without granting it authority over the admission transaction.
SetUiReady(ready) ==
    /\ ready \in BOOLEAN
    /\ ready # uiReady
    /\ uiReady' = ready
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    bindingState, liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline, now,
                    catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

\* Volume readiness is independent from UI readiness.  A new ready-volume
\* attempt clears only an unresolved transient observation; an exact-contract
\* or true-absence private reconciliation remains immutable.
SetProductionVolumeReady(ready) ==
    /\ ready \in BOOLEAN
    /\ ready # catalog.volumeReady
    /\ catalog' =
          [catalog EXCEPT
              !.volumeReady = ready,
              !.observation =
                  IF ready /\ catalog.privateState = PrivateQualificationPending
                     THEN CatalogUnobserved ELSE @]
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    bindingState, liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline, now,
                    uiReady, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

\* Ordinary signed catalog policy remains independent of the DVM volume and
\* UI readiness.  Its commit does not classify the optional private
\* qualification: reconciliation can still observe the exact contract later.
OrdinaryCatalogAdmissionEnabled ==
    /\ result = Pending
    /\ ~catalog.ordinaryCommitted

CommitOrdinaryCatalog ==
    /\ OrdinaryCatalogAdmissionEnabled
    /\ catalog' = [catalog EXCEPT !.ordinaryCommitted = TRUE]
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    bindingState, liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline, now,
                    uiReady, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

\* An unavailable production volume is neither a negative lookup nor a
\* private-reconciliation conclusion.  It records a retryable outcome and
\* leaves the exact qualification path pending for the next ready-volume probe.
ObserveTransientProductionVolumeUnavailable ==
    /\ result = Pending
    /\ bindingState = NotCreated
    /\ ~ProductionVolumeReady
    /\ catalog.privateState = PrivateQualificationPending
    /\ catalog.observation # CatalogVolumeTransientlyUnavailable
    /\ catalog' = [catalog EXCEPT
                       !.observation = CatalogVolumeTransientlyUnavailable]
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    bindingState, liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline, now,
                    uiReady, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

\* The private child is eligible only after the ready production volume has
\* returned the exact qualification contract, not merely after a catalog scan.
ObserveExactPrivateContract ==
    /\ result = Pending
    /\ bindingState = NotCreated
    /\ ProductionVolumeReady
    /\ catalog.contractTruth = ExactPrivateContract
    /\ catalog.privateState = PrivateQualificationPending
    /\ catalog' = [catalog EXCEPT
                       !.privateState = PrivateExactContractVisible,
                       !.observation = CatalogExactContractObserved,
                       !.executable = TrustedEarlySystemQualificationExecutable]
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    bindingState, liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline, now,
                    uiReady, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

\* A DVM may present bytes at the expected path, but it cannot acquire the
\* digest-verified early-system provenance.  The attempted replacement remains
\* explicit model state so admission without the provenance gate is observable.
AttemptDvmQualificationExecutableSubstitution ==
    /\ result = Pending
    /\ bindingState = NotCreated
    /\ ExactPrivateContractVisible
    /\ TrustedQualificationExecutableSelected
    /\ ~catalog.dvmSubstitutionAttempted
    /\ catalog' = [catalog EXCEPT
                       !.executable = DvmSubstitutedQualificationExecutable,
                       !.dvmSubstitutionAttempted = TRUE]
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    bindingState, liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline, now,
                    uiReady, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

\* A mounted-volume NotFound may settle the optional private path as absent.
\* This does not authorize the same conclusion from transient unavailability.
ResolvePrivateQualificationTrueAbsent ==
    /\ result = Pending
    /\ bindingState = NotCreated
    /\ ProductionVolumeReady
    /\ catalog.contractTruth = TrueAbsentPrivateContract
    /\ catalog.privateState = PrivateQualificationPending
    /\ catalog' = [catalog EXCEPT
                       !.privateState = PrivateContractAbsent,
                       !.observation = CatalogTrueAbsenceObserved]
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    bindingState, liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline, now,
                    uiReady, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

\* This models the authenticated SESSIOND bind syscall.  owner is the live
\* calling service/endpoint snapshot; target is the exact suspended child.
Bind(owner, target) ==
    /\ result = Pending
    /\ bindingState = Deferred
    /\ owner \in OwnerCandidates
    /\ target \in TargetCandidates
    /\ owner = SessiondAuthority
    /\ owner = liveOwnerAuthority
    /\ owner = deferredOwnerAuthority
    /\ target = liveTargetIdentity
    /\ target = deferredTargetIdentity
    /\ bindingState' = BoundSuspended
    /\ boundOwnerAuthority' = owner
    /\ boundTargetIdentity' = target
    /\ bindingId' = KernelBindingId
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    activationCount, activationTick, absoluteDeadline, now,
                    uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

\* Atomic authority consumption plus runnable publication.  No later action
\* changes activationTick or absoluteDeadline.
Activate ==
    /\ result = Pending
    /\ BindingIsLiveExact
    /\ activationCount = 0
    /\ now \in PreActivationTicks
    /\ bindingState' = Active
    /\ activationCount' = 1
    /\ activationTick' = now
    /\ absoluteDeadline' = now + QualifiedDeadline
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId, now,
                    uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

AdvancePreActivationTime ==
    /\ result = Pending
    /\ bindingState \in {NotCreated, Deferred, BoundSuspended}
    /\ now = 0
    /\ now' = 1
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    bindingState, liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline,
                    uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

AdvanceToDeadline ==
    /\ result = Pending
    /\ bindingState = Active
    /\ now < absoluteDeadline
    /\ now' = absoluteDeadline
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    bindingState, liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline,
                    uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

RecordReady(worker) ==
    /\ ActiveRecordAdmission(worker)
    /\ worker = NextReadyWorker
    /\ workerPhase[worker] = WorkerInit
    /\ evidence' = [evidence EXCEPT ![Ready][worker] = CorrectRecord(worker)]
    /\ workerPhase' = [workerPhase EXCEPT ![worker] = WorkerReady]
    /\ UNCHANGED <<workerCount, completed, bindingState, liveOwnerAuthority,
                    liveTargetIdentity, deferredOwnerAuthority,
                    deferredTargetIdentity, boundOwnerAuthority,
                    boundTargetIdentity, bindingId, activationCount,
                    activationTick, absoluteDeadline, now,
                    uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

DropReady(worker) ==
    /\ CriticalOutputDropAdmission(worker)
    /\ worker = 0
    /\ workerPhase[worker] = WorkerInit
    /\ evidence' = [evidence EXCEPT ![Ready][worker] = CorrectRecord(worker)]
    /\ workerPhase' = [workerPhase EXCEPT ![worker] = WorkerReady]
    /\ catalog' = [catalog EXCEPT
                       !.output.criticalDropped = TRUE,
                       !.output.qualificationMilestonesDropped = 1,
                       !.output.qualificationDebugBytesDiscarded = 1]
    /\ UNCHANGED <<workerCount, completed, bindingState, liveOwnerAuthority,
                    liveTargetIdentity, deferredOwnerAuthority,
                    deferredTargetIdentity, boundOwnerAuthority,
                    boundTargetIdentity, bindingId, activationCount,
                    activationTick, absoluteDeadline, now,
                    uiReady, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

RecordStart(worker) ==
    /\ ActiveRecordAdmission(worker)
    /\ AllReady
    /\ worker = NextStartedWorker
    /\ workerPhase[worker] = WorkerReady
    /\ evidence' = [evidence EXCEPT ![Start][worker] = CorrectRecord(worker)]
    /\ workerPhase' = [workerPhase EXCEPT ![worker] = WorkerStarted]
    /\ UNCHANGED <<workerCount, completed, bindingState, liveOwnerAuthority,
                    liveTargetIdentity, deferredOwnerAuthority,
                    deferredTargetIdentity, boundOwnerAuthority,
                    boundTargetIdentity, bindingId, activationCount,
                    activationTick, absoluteDeadline, now,
                    uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

DropStart(worker) ==
    /\ CriticalOutputDropAdmission(worker)
    /\ AllReady
    /\ worker = 0
    /\ workerPhase[worker] = WorkerReady
    /\ evidence' = [evidence EXCEPT ![Start][worker] = CorrectRecord(worker)]
    /\ workerPhase' = [workerPhase EXCEPT ![worker] = WorkerStarted]
    /\ catalog' = [catalog EXCEPT
                       !.output.criticalDropped = TRUE,
                       !.output.qualificationMilestonesDropped = 1,
                       !.output.qualificationDebugBytesDiscarded = 1]
    /\ UNCHANGED <<workerCount, completed, bindingState, liveOwnerAuthority,
                    liveTargetIdentity, deferredOwnerAuthority,
                    deferredTargetIdentity, boundOwnerAuthority,
                    boundTargetIdentity, bindingId, activationCount,
                    activationTick, absoluteDeadline, now,
                    uiReady, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

CompleteBoundedWork(worker) ==
    /\ ActiveRecordAdmission(worker)
    /\ worker = NextCompletedWorker
    /\ workerPhase[worker] = WorkerStarted
    /\ worker \notin CompletedWorkers
    /\ completed' = completed \cup {worker}
    /\ UNCHANGED <<workerCount, evidence, workerPhase, bindingState,
                    liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline, now,
                    uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

RecordFinish(worker) ==
    /\ FinishAdmission(worker)
    /\ worker = NextFinishedWorker
    /\ evidence' = [evidence EXCEPT ![Finish][worker] = CorrectRecord(worker)]
    /\ workerPhase' = [workerPhase EXCEPT ![worker] = WorkerFinished]
    /\ UNCHANGED <<workerCount, completed, bindingState, liveOwnerAuthority,
                    liveTargetIdentity, deferredOwnerAuthority,
                    deferredTargetIdentity, boundOwnerAuthority,
                    boundTargetIdentity, bindingId, activationCount,
                    activationTick, absoluteDeadline, now,
                    uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

DropFinish(worker) ==
    /\ CriticalOutputDropAdmission(worker)
    /\ AllWorkCompleted
    /\ worker = 0
    /\ workerPhase[worker] = WorkerStarted
    /\ evidence' = [evidence EXCEPT ![Finish][worker] = CorrectRecord(worker)]
    /\ workerPhase' = [workerPhase EXCEPT ![worker] = WorkerFinished]
    /\ catalog' = [catalog EXCEPT
                       !.output.criticalDropped = TRUE,
                       !.output.qualificationMilestonesDropped = 1,
                       !.output.qualificationDebugBytesDiscarded = 1]
    /\ UNCHANGED <<workerCount, completed, bindingState, liveOwnerAuthority,
                    liveTargetIdentity, deferredOwnerAuthority,
                    deferredTargetIdentity, boundOwnerAuthority,
                    boundTargetIdentity, bindingId, activationCount,
                    activationTick, absoluteDeadline, now,
                    uiReady, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

JoinWorker(worker) ==
    /\ ActiveRecordAdmission(worker)
    /\ worker = NextJoinedWorker
    /\ workerPhase[worker] = WorkerFinished
    /\ workerPhase' = [workerPhase EXCEPT ![worker] = WorkerJoined]
    /\ UNCHANGED <<workerCount, evidence, completed, bindingState,
                    liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline, now,
                    uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

RecordComplete ==
    /\ ActiveRecordAdmission(0)
    /\ AllJoined
    /\ ~TerminalRecorded
    /\ evidence' = [evidence EXCEPT ![Complete][0] = CorrectRecord(0)]
    /\ UNCHANGED <<workerCount, completed, workerPhase, bindingState,
                    liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline, now,
                    uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

DropComplete ==
    /\ CriticalOutputDropAdmission(0)
    /\ AllJoined
    /\ ~TerminalRecorded
    /\ evidence' = [evidence EXCEPT ![Complete][0] = CorrectRecord(0)]
    /\ catalog' = [catalog EXCEPT
                       !.output.criticalDropped = TRUE,
                       !.output.qualificationMilestonesDropped = 1,
                       !.output.qualificationDebugBytesDiscarded = 1]
    /\ UNCHANGED <<workerCount, completed, workerPhase, bindingState,
                    liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline, now,
                    uiReady, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

\* Scheduler measurement output is one-shot and ordinary.  It may occur
\* before or after a critical loss, but it never mutates qualification-local
\* counters; the first critical loss remains the sole acceptance boundary.
DropSchedulerMeasurement ==
    /\ ActiveRecordAdmission(0)
    /\ ~output.schedulerMeasurementDropped
    /\ catalog' = [catalog EXCEPT !.output.schedulerMeasurementDropped = TRUE]
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase, bindingState,
                    liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline, now,
                    uiReady, duplicateEvidenceAccepted, expiredEvidenceAccepted, result>>

Succeed ==
    /\ result = Pending
    /\ bindingState = Active
    /\ TerminalRecorded
    /\ QualificationEvidenceEventCount = 3 * workerCount + 1
    /\ QualificationOutputClean
    /\ bindingState' = Terminal
    /\ result' = Successful
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline, now,
                    uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted>>

Expire ==
    /\ result = Pending
    /\ bindingState = Active
    /\ now >= absoluteDeadline
    /\ bindingState' = Terminal
    /\ result' = TimedOut
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    liveOwnerAuthority, liveTargetIdentity,
                    deferredOwnerAuthority, deferredTargetIdentity,
                    boundOwnerAuthority, boundTargetIdentity, bindingId,
                    activationCount, activationTick, absoluteDeadline, now,
                    uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted>>

\* Endpoint epoch change or SESSIOND replacement revokes every registered
\* bound authority before a later process can use it.
OwnerRevoke ==
    /\ result = Pending
    /\ bindingState \in {BoundSuspended, Active}
    /\ liveOwnerAuthority = SessiondAuthority
    /\ liveOwnerAuthority' = ReusedSessiondAuthority
    /\ bindingState' = Revoked
    /\ result' = RevokedResult
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    liveTargetIdentity, deferredOwnerAuthority,
                    deferredTargetIdentity, boundOwnerAuthority,
                    boundTargetIdentity, bindingId, activationCount,
                    activationTick, absoluteDeadline, now,
                    uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted>>

\* next represents target exit, exec/mm replacement, or a reused PID.  The
\* stored binding remains trace metadata, but it is no longer an active map
\* entry and cannot admit a subsequent milestone.
TargetInvalidate(next) ==
    /\ result = Pending
    /\ bindingState \in {BoundSuspended, Active}
    /\ next \in TargetInvalidations
    /\ liveTargetIdentity' = next
    /\ bindingState' = Revoked
    /\ result' = RevokedResult
    /\ UNCHANGED <<workerCount, evidence, completed, workerPhase,
                    liveOwnerAuthority, deferredOwnerAuthority,
                    deferredTargetIdentity, boundOwnerAuthority,
                    boundTargetIdentity, bindingId, activationCount,
                    activationTick, absoluteDeadline, now,
                    uiReady, catalog, duplicateEvidenceAccepted, expiredEvidenceAccepted>>

\* These malformed host-parser inputs are intentionally stuttering actions.
\* Mutation tests turn one into a state change and require the named safety
\* invariant to reject it.
RejectDuplicateEvidence ==
    /\ result = Pending
    /\ \E worker \in ExpectedWorkers:
          Recorded(Ready, worker) \/ Recorded(Start, worker) \/ Recorded(Finish, worker)
    /\ UNCHANGED vars

RejectForeignWorkerEvidence ==
    /\ result = Pending
    /\ bindingState = Active
    /\ workerCount < MaxWorkers
    /\ UNCHANGED vars

RejectExpiredEvidence ==
    /\ result = Pending
    /\ bindingState = Active
    /\ now >= absoluteDeadline
    /\ UNCHANGED vars

RejectDeadlineRefresh ==
    /\ result = Pending
    /\ bindingState = Active
    /\ UNCHANGED vars

TerminalStutter ==
    /\ result \in {Successful, TimedOut, RevokedResult}
    /\ UNCHANGED vars

Next ==
    \/ CreateDeferredChild
    \/ \E ready \in BOOLEAN: SetUiReady(ready)
    \/ \E ready \in BOOLEAN: SetProductionVolumeReady(ready)
    \/ CommitOrdinaryCatalog
    \/ ObserveTransientProductionVolumeUnavailable
    \/ ObserveExactPrivateContract
    \/ AttemptDvmQualificationExecutableSubstitution
    \/ ResolvePrivateQualificationTrueAbsent
    \/ \E owner \in OwnerCandidates, target \in TargetCandidates: Bind(owner, target)
    \/ Activate
    \/ AdvancePreActivationTime
    \/ AdvanceToDeadline
    \/ \E worker \in WorkerIds: RecordReady(worker)
    \/ \E worker \in WorkerIds: DropReady(worker)
    \/ \E worker \in WorkerIds: RecordStart(worker)
    \/ \E worker \in WorkerIds: DropStart(worker)
    \/ \E worker \in WorkerIds: CompleteBoundedWork(worker)
    \/ \E worker \in WorkerIds: RecordFinish(worker)
    \/ \E worker \in WorkerIds: DropFinish(worker)
    \/ \E worker \in WorkerIds: JoinWorker(worker)
    \/ RecordComplete
    \/ DropComplete
    \/ DropSchedulerMeasurement
    \/ Succeed
    \/ Expire
    \/ OwnerRevoke
    \/ \E next \in TargetInvalidations: TargetInvalidate(next)
    \/ RejectDuplicateEvidence
    \/ RejectForeignWorkerEvidence
    \/ RejectExpiredEvidence
    \/ RejectDeadlineRefresh
    \/ TerminalStutter

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ \A worker \in WorkerIds:
          /\ WF_vars(RecordReady(worker))
          /\ WF_vars(RecordStart(worker))
          /\ WF_vars(CompleteBoundedWork(worker))
          /\ WF_vars(RecordFinish(worker))
          /\ WF_vars(JoinWorker(worker))
    /\ WF_vars(RecordComplete)
    /\ WF_vars(Succeed)
    /\ WF_vars(AdvanceToDeadline)
    /\ WF_vars(Expire)

TypeOK ==
    /\ workerCount \in SupportedCounts
    /\ evidence \in [Phases -> [WorkerIds -> RecordValues \cup {NoRecord}]]
    /\ completed \subseteq ExpectedWorkers
    /\ workerPhase \in [WorkerIds -> WorkerStates]
    /\ bindingState \in BindingStates
    /\ liveOwnerAuthority \in OwnerAuthorityValues
    /\ liveTargetIdentity \in IdentityValues
    /\ deferredOwnerAuthority \in OwnerAuthorityValues
    /\ deferredTargetIdentity \in IdentityValues
    /\ boundOwnerAuthority \in OwnerAuthorityValues
    /\ boundTargetIdentity \in IdentityValues
    /\ bindingId \in BindingIdValues
    /\ activationCount \in 0..1
    /\ activationTick \in PreActivationTicks
    /\ absoluteDeadline \in AbsoluteDeadlineValues
    /\ now \in TimeValues
    /\ uiReady \in BOOLEAN
    /\ catalog \in CatalogRecords
    /\ duplicateEvidenceAccepted \in BOOLEAN
    /\ expiredEvidenceAccepted \in BOOLEAN
    /\ result \in Results

BindingLifecycleIsExact ==
    /\ bindingState = NotCreated =>
          /\ deferredOwnerAuthority = NoOwnerAuthority
          /\ deferredTargetIdentity = NoIdentity
          /\ boundOwnerAuthority = NoOwnerAuthority
          /\ boundTargetIdentity = NoIdentity
          /\ bindingId = NoBindingId
          /\ activationCount = 0
          /\ absoluteDeadline = NoDeadline
    /\ bindingState = Deferred =>
          /\ deferredOwnerAuthority = SessiondAuthority
          /\ deferredTargetIdentity = QualificationIdentity
          /\ boundOwnerAuthority = NoOwnerAuthority
          /\ boundTargetIdentity = NoIdentity
          /\ bindingId = NoBindingId
          /\ activationCount = 0
          /\ absoluteDeadline = NoDeadline
    /\ bindingState \in {BoundSuspended, Active} =>
          /\ deferredOwnerAuthority = SessiondAuthority
          /\ deferredTargetIdentity = QualificationIdentity
          /\ boundOwnerAuthority = SessiondAuthority
          /\ boundTargetIdentity = QualificationIdentity
          /\ bindingId = KernelBindingId

ActiveBindingMatchesLiveOwnerAndTarget ==
    bindingState = Active =>
        /\ boundOwnerAuthority = liveOwnerAuthority
        /\ boundTargetIdentity = liveTargetIdentity
        /\ boundOwnerAuthority = SessiondAuthority
        /\ boundTargetIdentity = QualificationIdentity

BindBeforeActivationAndNoRebind ==
    /\ activationCount = 1 =>
          /\ bindingId = KernelBindingId
          /\ boundOwnerAuthority = SessiondAuthority
          /\ boundTargetIdentity = QualificationIdentity
          /\ bindingState \notin {NotCreated, Deferred, BoundSuspended}
    /\ bindingState = BoundSuspended => activationCount = 0

ActivationStampsImmutableAbsoluteDeadline ==
    /\ activationCount = 0 =>
          /\ activationTick = NoDeadline
          /\ absoluteDeadline = NoDeadline
    /\ activationCount = 1 =>
          /\ absoluteDeadline = activationTick + QualifiedDeadline
          /\ absoluteDeadline # NoDeadline

\* The private KVM qualification is a scheduler/core proof.  It has no
\* display-provider dependency: once the ready production volume has exposed
\* the exact contract, an unavailable UI must still leave deferred-child
\* admission enabled after the bounded pre-activation clock advance.
PrivateDeferredChildAdmissionIsUiIndependent ==
    (~uiReady /\ result = Pending /\ bindingState = NotCreated /\ now = 1
     /\ ProductionVolumeReady /\ ExactPrivateContractVisible
     /\ TrustedQualificationExecutableSelected)
        => PrivateDeferredChildAdmissionEnabled

\* The ordinary signed catalog is an initd/UI policy path.  Its commit must
\* remain enabled even when both UI and the optional DVM volume are down.
OrdinaryCatalogAdmissionIsStorageAndUiIndependent ==
    (~uiReady /\ ~ProductionVolumeReady /\ result = Pending
     /\ ~catalog.ordinaryCommitted) => OrdinaryCatalogAdmissionEnabled

\* A transient volume result carries no absence authority.  It cannot settle
\* the private reconciliation or create the deferred qualification child.
TransientProductionVolumeUnavailabilityRemainsPending ==
    catalog.observation = CatalogVolumeTransientlyUnavailable =>
        /\ catalog.privateState = PrivateQualificationPending
        /\ bindingState = NotCreated
        /\ result = Pending

\* Both private reconciliation conclusions are provenance-bearing.  Exact
\* visibility and true mounted-volume NotFound are the only terminal outcomes.
PrivateQualificationConclusionHasExactVolumeProvenance ==
    /\ catalog.privateState = PrivateExactContractVisible =>
          /\ catalog.contractTruth = ExactPrivateContract
          /\ catalog.observation = CatalogExactContractObserved
    /\ catalog.privateState = PrivateContractAbsent =>
          /\ catalog.contractTruth = TrueAbsentPrivateContract
          /\ catalog.observation = CatalogTrueAbsenceObserved

\* The enabled private path needs a currently ready production volume and a
\* visible exact contract.  The child retains the observed contract identity
\* after creation, even if a later environment event drops volume readiness.
PrivateDeferredChildAdmissionRequiresReadyExactProductionContract ==
    PrivateDeferredChildAdmissionEnabled =>
        /\ ProductionVolumeReady
        /\ ExactPrivateContractVisible

\* Matching the path is insufficient: only the digest-verified early-system
\* executable may become the private deferred child.  A DVM candidate with the
\* same path must remain non-admissible and must not create binding authority.
AttemptedDvmExecutableSubstitutionNeverEnablesPrivateAdmission ==
    (catalog.dvmSubstitutionAttempted /\
     catalog.executable = DvmSubstitutedQualificationExecutable) =>
        /\ ~PrivateDeferredChildAdmissionEnabled
        /\ bindingState = NotCreated

PrivateChildCreationHasExactPrivateContract ==
    bindingState # NotCreated => ExactPrivateContractVisible

PrivateChildCreationUsesTrustedEarlySystemExecutable ==
    bindingState # NotCreated => TrustedQualificationExecutableSelected

TrueAbsentPrivateContractLeavesOrdinaryCatalogAvailable ==
    (catalog.privateState = PrivateContractAbsent /\ ~catalog.ordinaryCommitted) =>
        /\ OrdinaryCatalogAdmissionEnabled
        /\ bindingState = NotCreated

ExactWorkerCpuBijectionAndStampedIdentity ==
    /\ \A phase \in Phases, worker \in WorkerIds:
          LET record == evidence[phase][worker] IN
              record # NoRecord =>
                  /\ worker \in ExpectedWorkers
                  /\ phase = Complete => worker = 0
                  /\ record.worker = worker
                  /\ record.cpu = worker
                  /\ record.pid = QualificationPid
                  /\ record.tid = worker + 1
    /\ \A left \in ExpectedWorkers, right \in ExpectedWorkers:
          left # right => left + 1 # right + 1

ReadyBarrierPrecedesEveryStart ==
    StartedWorkers # {} => AllReady

FinishRequiresOwnStartAndGlobalBoundedWork ==
    /\ FinishedWorkers \subseteq StartedWorkers
    /\ FinishedWorkers \subseteq CompletedWorkers
    /\ FinishedWorkers # {} => AllWorkCompleted

WorkerFsmAndJoinBeforeComplete ==
    /\ \A worker \in ExpectedWorkers:
          /\ workerPhase[worker] \in {WorkerInit, WorkerReady, WorkerStarted,
                                       WorkerFinished, WorkerJoined}
          /\ workerPhase[worker] \in {WorkerReady, WorkerStarted, WorkerFinished,
                                       WorkerJoined} => Recorded(Ready, worker)
          /\ workerPhase[worker] \in {WorkerStarted, WorkerFinished, WorkerJoined} =>
                Recorded(Start, worker)
          /\ workerPhase[worker] \in {WorkerFinished, WorkerJoined} =>
                Recorded(Finish, worker)
    /\ TerminalRecorded => AllJoined

PhaseMetadataIsImmutableAndBounded ==
    /\ \A phase \in Phases, worker \in WorkerIds:
          LET record == evidence[phase][worker] IN
              record # NoRecord => ExactRecordFor(worker, record)
    /\ \A worker \in ExpectedWorkers:
          \A left \in Phases, right \in Phases:
              /\ Recorded(left, worker)
              /\ Recorded(right, worker)
              => /\ evidence[left][worker].work = evidence[right][worker].work
                 /\ evidence[left][worker].deadline = evidence[right][worker].deadline
                 /\ evidence[left][worker].binding_id = evidence[right][worker].binding_id

NoAcceptedEvidenceAtOrAfterDeadline ==
    \A record \in AcceptedRecords: record.at < record.deadline

NonzeroUniformKernelBindingId ==
    AcceptedRecords # {} =>
        /\ bindingId = KernelBindingId
        /\ AcceptedBindingIds = {KernelBindingId}

OwnerOrTargetCleanupRemovesActiveBinding ==
    (liveOwnerAuthority # boundOwnerAuthority \/
     liveTargetIdentity # boundTargetIdentity) => bindingState # Active

MalformedEvidenceIsNeverAccepted ==
    /\ ~duplicateEvidenceAccepted
    /\ ~expiredEvidenceAccepted

\* A qualification-critical drop is terminal for acceptance, while a scheduler
\* measurement drop carries no qualification-local loss.  The compact state
\* records source-visible counter class rather than the per-frame transport.
QualificationCriticalOutputAccountingIsExact ==
    /\ output.criticalDropped =>
          /\ output.qualificationMilestonesDropped = 1
          /\ output.qualificationDebugBytesDiscarded = 1
    /\ ~output.criticalDropped =>
          /\ output.qualificationMilestonesDropped = 0
          /\ output.qualificationDebugBytesDiscarded = 0

SchedulerMeasurementLossDoesNotTaintQualificationAccounting ==
    (output.schedulerMeasurementDropped /\ ~output.criticalDropped) =>
        QualificationOutputClean

TerminalCompleteFollowsEveryWorkerJoin ==
    TerminalRecorded =>
        /\ AllFinished
        /\ AllJoined
        /\ ExactRecordFor(0, evidence[Complete][0])
        /\ \A worker \in ExpectedWorkers \ {0}: ~Recorded(Complete, worker)

SuccessfulQualificationHasEveryExactEvidenceRecord ==
    result = Successful =>
        /\ bindingState = Terminal
        /\ AllReady
        /\ StartedWorkers = ExpectedWorkers
        /\ CompletedWorkers = ExpectedWorkers
        /\ AllFinished
        /\ AllJoined
        /\ TerminalRecorded
        /\ NonzeroUniformKernelBindingId
        /\ \A phase \in WorkerPhases, worker \in ExpectedWorkers:
              ExactRecordFor(worker, evidence[phase][worker])

SuccessfulQualificationHasCleanExactOutput ==
    result = Successful =>
        /\ QualificationOutputClean
        /\ QualificationEvidenceEventCount = 3 * workerCount + 1

AllActiveWorkersEventuallySettleUnderBoundedFairness ==
    [] ((bindingState = Active /\ result = Pending) => <> (result # Pending))

=============================================================================
