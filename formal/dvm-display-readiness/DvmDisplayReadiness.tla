------------------------- MODULE DvmDisplayReadiness -------------------------
EXTENDS Naturals

(*******************************************************************************
Models process ownership and local readiness for the Linux DVM KMS/GPU relay.

Concrete owner and source anchors:
  * driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-display.c
    claim_display_process_owner, publish_display_ready_lock, serve_gpu_display,
    serve_display, and main
  * rustos-dvm-agent.c display_relay_is_ready

The ready linearization point is renameat(.display-ready.next,
display-ready.lock), after the exact payload is complete, fsync'd, and locked.
An ordinary relay fault closes the ready lock before scheduler restoration.
SIGKILL and RLIMIT_RTTIME termination release the process owner and ready locks.
The fixed candidate name and process singleton bound crash residue and reject a
second relay process.
*******************************************************************************)

CONSTANTS MaxRetries, MaxDuplicateAttempts

AwaitHost == "await-host"
RelayActive == "relay-active"
PublishingEmpty == "publishing-empty"
PublishingComplete == "publishing-complete"
Ready == "ready"
Teardown == "teardown"
Retry == "retry"
Terminated == "terminated"

Normal == "normal"
RoundRobin == "round-robin"
Restoring == "restoring"
NoProcess == "no-process"

NoLock == "none"
RelayLock == "relay"

VARIABLES phase,
          scheduler,
          peerConfirmed,
          ownerLock,
          candidatePresent,
          candidateComplete,
          candidateLock,
          readyPathExact,
          readyLock,
          publishEpoch,
          retries,
          duplicateAttempts,
          restoreFailed

vars == <<phase, scheduler, peerConfirmed, ownerLock, candidatePresent,
          candidateComplete, candidateLock, readyPathExact, readyLock,
          publishEpoch, retries, duplicateAttempts, restoreFailed>>

ProcessAlive == phase # Terminated

LocalDisplayHealthWouldSucceed ==
    /\ readyPathExact
    /\ readyLock = RelayLock

Init ==
    /\ phase = AwaitHost
    /\ scheduler = Normal
    /\ peerConfirmed = FALSE
    /\ ownerLock = RelayLock
    /\ candidatePresent = FALSE
    /\ candidateComplete = FALSE
    /\ candidateLock = NoLock
    /\ readyPathExact = FALSE
    /\ readyLock = NoLock
    /\ publishEpoch = 0
    /\ retries = 0
    /\ duplicateAttempts = 0
    /\ restoreFailed = FALSE

ConfirmPeerAndAdmitScheduler ==
    /\ phase = AwaitHost
    /\ scheduler = Normal
    /\ ownerLock = RelayLock
    /\ phase' = RelayActive
    /\ scheduler' = RoundRobin
    /\ peerConfirmed' = TRUE
    /\ UNCHANGED <<ownerLock, candidatePresent, candidateComplete,
                    candidateLock, readyPathExact, readyLock, publishEpoch,
                    retries, duplicateAttempts, restoreFailed>>

BeginReadyPublication ==
    /\ phase = RelayActive
    /\ scheduler = RoundRobin
    /\ peerConfirmed
    /\ ownerLock = RelayLock
    /\ readyLock = NoLock
    /\ phase' = PublishingEmpty
    /\ candidatePresent' = TRUE
    /\ candidateComplete' = FALSE
    /\ candidateLock' = RelayLock
    /\ UNCHANGED <<scheduler, peerConfirmed, ownerLock, readyPathExact,
                    readyLock, publishEpoch, retries, duplicateAttempts,
                    restoreFailed>>

CompleteReadyCandidate ==
    /\ phase = PublishingEmpty
    /\ candidatePresent
    /\ candidateLock = RelayLock
    /\ phase' = PublishingComplete
    /\ candidateComplete' = TRUE
    /\ UNCHANGED <<scheduler, peerConfirmed, ownerLock, candidatePresent,
                    candidateLock, readyPathExact, readyLock, publishEpoch,
                    retries, duplicateAttempts, restoreFailed>>

AtomicPublishReady ==
    /\ phase = PublishingComplete
    /\ scheduler = RoundRobin
    /\ peerConfirmed
    /\ ownerLock = RelayLock
    /\ candidatePresent
    /\ candidateComplete
    /\ candidateLock = RelayLock
    /\ phase' = Ready
    /\ candidatePresent' = FALSE
    /\ candidateComplete' = FALSE
    /\ candidateLock' = NoLock
    /\ readyPathExact' = TRUE
    /\ readyLock' = RelayLock
    /\ publishEpoch' = publishEpoch + 1
    /\ UNCHANGED <<scheduler, peerConfirmed, ownerLock, retries,
                    duplicateAttempts, restoreFailed>>

DuplicateProcessRejected ==
    /\ ProcessAlive
    /\ ownerLock = RelayLock
    /\ duplicateAttempts < MaxDuplicateAttempts
    /\ duplicateAttempts' = duplicateAttempts + 1
    /\ UNCHANGED <<phase, scheduler, peerConfirmed, ownerLock,
                    candidatePresent, candidateComplete, candidateLock,
                    readyPathExact, readyLock, publishEpoch, retries,
                    restoreFailed>>

RelayFault ==
    /\ phase \in {RelayActive, PublishingEmpty, PublishingComplete, Ready}
    /\ scheduler = RoundRobin
    /\ phase' = Teardown
    /\ scheduler' = Restoring
    /\ peerConfirmed' = FALSE
    \* Readiness is withdrawn before the potentially failing scheduler restore.
    /\ readyLock' = NoLock
    /\ candidateLock' = NoLock
    /\ UNCHANGED <<ownerLock, candidatePresent, candidateComplete,
                    readyPathExact, publishEpoch, retries, duplicateAttempts,
                    restoreFailed>>

RestoreScheduler ==
    /\ phase = Teardown
    /\ scheduler = Restoring
    /\ phase' = Retry
    /\ scheduler' = Normal
    /\ candidatePresent' = FALSE
    /\ candidateComplete' = FALSE
    /\ restoreFailed' = FALSE
    /\ UNCHANGED <<peerConfirmed, ownerLock, candidateLock, readyPathExact,
                    readyLock, publishEpoch, retries, duplicateAttempts>>

RetryRelay ==
    /\ phase = Retry
    /\ scheduler = Normal
    /\ retries < MaxRetries
    /\ phase' = AwaitHost
    /\ retries' = retries + 1
    /\ UNCHANGED <<scheduler, peerConfirmed, ownerLock, candidatePresent,
                    candidateComplete, candidateLock, readyPathExact, readyLock,
                    publishEpoch, duplicateAttempts, restoreFailed>>

FatalRestore ==
    /\ phase = Teardown
    /\ scheduler = Restoring
    /\ phase' = Terminated
    /\ scheduler' = NoProcess
    /\ ownerLock' = NoLock
    /\ candidateLock' = NoLock
    /\ readyLock' = NoLock
    /\ restoreFailed' = TRUE
    /\ UNCHANGED <<peerConfirmed, candidatePresent, candidateComplete,
                    readyPathExact, publishEpoch, retries, duplicateAttempts>>

HardLimitOrCrash ==
    /\ ProcessAlive
    /\ phase' = Terminated
    /\ scheduler' = NoProcess
    /\ peerConfirmed' = FALSE
    /\ ownerLock' = NoLock
    /\ candidateLock' = NoLock
    /\ readyLock' = NoLock
    /\ UNCHANGED <<candidatePresent, candidateComplete, readyPathExact,
                    publishEpoch, retries, duplicateAttempts, restoreFailed>>

Next ==
    \/ ConfirmPeerAndAdmitScheduler
    \/ BeginReadyPublication
    \/ CompleteReadyCandidate
    \/ AtomicPublishReady
    \/ DuplicateProcessRejected
    \/ RelayFault
    \/ RestoreScheduler
    \/ RetryRelay
    \/ FatalRestore
    \/ HardLimitOrCrash

TypeOK ==
    /\ phase \in {AwaitHost, RelayActive, PublishingEmpty,
                   PublishingComplete, Ready, Teardown, Retry, Terminated}
    /\ scheduler \in {Normal, RoundRobin, Restoring, NoProcess}
    /\ peerConfirmed \in BOOLEAN
    /\ ownerLock \in {NoLock, RelayLock}
    /\ candidatePresent \in BOOLEAN
    /\ candidateComplete \in BOOLEAN
    /\ candidateLock \in {NoLock, RelayLock}
    /\ readyPathExact \in BOOLEAN
    /\ readyLock \in {NoLock, RelayLock}
    /\ publishEpoch \in 0..(MaxRetries + 1)
    /\ retries \in 0..MaxRetries
    /\ duplicateAttempts \in 0..MaxDuplicateAttempts
    /\ restoreFailed \in BOOLEAN

HealthRequiresLiveAuthenticatedRelay ==
    LocalDisplayHealthWouldSucceed =>
        /\ phase = Ready
        /\ ProcessAlive
        /\ scheduler = RoundRobin
        /\ peerConfirmed
        /\ ownerLock = RelayLock
        /\ publishEpoch > 0

CandidateCannotPublishPartialReadiness ==
    phase \in {PublishingEmpty, PublishingComplete} =>
        /\ readyLock = NoLock
        /\ ~LocalDisplayHealthWouldSucceed

TeardownWithdrawsReadinessBeforeRestore ==
    phase = Teardown =>
        /\ scheduler = Restoring
        /\ readyLock = NoLock
        /\ ~LocalDisplayHealthWouldSucceed

TerminatedProcessHasNoReadinessAuthority ==
    phase = Terminated =>
        /\ scheduler = NoProcess
        /\ ownerLock = NoLock
        /\ candidateLock = NoLock
        /\ readyLock = NoLock
        /\ ~LocalDisplayHealthWouldSucceed

SingletonOwnsEveryPublication ==
    readyLock = RelayLock \/ candidateLock = RelayLock => ownerLock = RelayLock

CrashResidueIsBounded ==
    candidatePresent \in BOOLEAN

SettleTeardown == RestoreScheduler \/ FatalRestore \/ HardLimitOrCrash

EveryTeardownSettles ==
    phase = Teardown ~> phase # Teardown

Spec == Init /\ [][Next]_vars /\ WF_vars(SettleTeardown)

=============================================================================
