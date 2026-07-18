--------------------------- MODULE DvmAgentReadiness ---------------------------
EXTENDS Naturals

(*******************************************************************************
Models process-owned local readiness for the Linux DVM control agent.

Concrete owner and source anchors:
  * driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c
    open_ready_directory, publish_ready, local_health, serve, and main
  * driver-domains/linux/board/overlay/etc/init.d/S50rustos-dvm

The linearization point is renameat(.ready.next, ready): the candidate inode is
already complete and exclusively locked, so a reader sees either the old inode
or the complete new inode. The serving process retains both the singleton and
ready-inode locks for its lifetime. Process exit releases both locks even when
the exact ready payload remains in the filesystem. The diagnostic announce
command never publishes readiness.
*******************************************************************************)

CONSTANTS MaxRestarts, MaxAnnouncements

Offline == "offline"
Starting == "starting"
Initialized == "initialized"
Publishing == "publishing"
Serving == "serving"
Exited == "exited"

Absent == "absent"
Regular == "regular"
Symlink == "symlink"
Other == "other"

Empty == "empty"
Exact == "exact"
Malformed == "malformed"

NoLock == "none"
AgentLock == "agent"

VARIABLES phase,
          directorySafe,
          initializationComplete,
          pathKind,
          pathPayload,
          readyLock,
          singletonLock,
          candidatePresent,
          candidatePayload,
          candidateLock,
          publishEpoch,
          restarts,
          announcements

vars == <<phase, directorySafe, initializationComplete, pathKind, pathPayload,
          readyLock, singletonLock, candidatePresent, candidatePayload,
          candidateLock, publishEpoch, restarts, announcements>>

AgentAlive == phase \in {Starting, Initialized, Publishing, Serving}

LocalHealthWouldSucceed ==
    /\ directorySafe
    /\ pathKind = Regular
    /\ pathPayload = Exact
    /\ readyLock = AgentLock

Init ==
    /\ phase = Offline
    /\ directorySafe = TRUE
    /\ initializationComplete = FALSE
    /\ pathKind = Absent
    /\ pathPayload = Empty
    /\ readyLock = NoLock
    /\ singletonLock = NoLock
    /\ candidatePresent = FALSE
    /\ candidatePayload = Empty
    /\ candidateLock = NoLock
    /\ publishEpoch = 0
    /\ restarts = 0
    /\ announcements = 0

Start ==
    /\ phase \in {Offline, Exited}
    /\ restarts < MaxRestarts
    /\ phase' = Starting
    /\ initializationComplete' = FALSE
    /\ readyLock' = NoLock
    /\ singletonLock' = NoLock
    /\ candidateLock' = NoLock
    /\ restarts' = restarts + 1
    /\ UNCHANGED <<directorySafe, pathKind, pathPayload, candidatePresent,
                    candidatePayload, publishEpoch, announcements>>

FinishInitialization ==
    /\ phase = Starting
    /\ phase' = Initialized
    /\ initializationComplete' = TRUE
    /\ UNCHANGED <<directorySafe, pathKind, pathPayload, readyLock,
                    singletonLock, candidatePresent, candidatePayload,
                    candidateLock, publishEpoch, restarts, announcements>>

BeginPublication ==
    /\ phase = Initialized
    /\ directorySafe
    /\ readyLock = NoLock
    /\ phase' = Publishing
    /\ singletonLock' = AgentLock
    \* The one fixed candidate name is removed and recreated while the
    \* singleton lock is held, so crash residue cannot accumulate.
    /\ candidatePresent' = TRUE
    /\ candidatePayload' = Empty
    /\ candidateLock' = AgentLock
    /\ UNCHANGED <<directorySafe, initializationComplete, pathKind,
                    pathPayload, readyLock, publishEpoch, restarts, announcements>>

WriteCandidate ==
    /\ phase = Publishing
    /\ candidatePresent
    /\ candidateLock = AgentLock
    /\ candidatePayload = Empty
    /\ candidatePayload' = Exact
    /\ UNCHANGED <<phase, directorySafe, initializationComplete, pathKind,
                    pathPayload, readyLock, singletonLock, candidatePresent,
                    candidateLock, publishEpoch, restarts, announcements>>

AtomicInstall ==
    /\ phase = Publishing
    /\ initializationComplete
    /\ singletonLock = AgentLock
    /\ candidatePresent
    /\ candidatePayload = Exact
    /\ candidateLock = AgentLock
    /\ phase' = Serving
    /\ pathKind' = Regular
    /\ pathPayload' = Exact
    /\ readyLock' = AgentLock
    /\ candidatePresent' = FALSE
    /\ candidatePayload' = Empty
    /\ candidateLock' = NoLock
    /\ publishEpoch' = publishEpoch + 1
    /\ UNCHANGED <<directorySafe, initializationComplete, singletonLock,
                    restarts, announcements>>

RejectUnsafeDirectory ==
    /\ phase = Initialized
    /\ ~directorySafe
    /\ phase' = Exited
    /\ initializationComplete' = FALSE
    /\ UNCHANGED <<directorySafe, pathKind, pathPayload, readyLock,
                    singletonLock, candidatePresent, candidatePayload,
                    candidateLock, publishEpoch, restarts, announcements>>

PublishFailure ==
    /\ phase = Publishing
    /\ phase' = Exited
    /\ initializationComplete' = FALSE
    /\ readyLock' = NoLock
    /\ singletonLock' = NoLock
    /\ candidatePresent' = FALSE
    /\ candidatePayload' = Empty
    /\ candidateLock' = NoLock
    /\ UNCHANGED <<directorySafe, pathKind, pathPayload, publishEpoch,
                    restarts, announcements>>

Crash ==
    /\ AgentAlive
    /\ phase' = Exited
    /\ initializationComplete' = FALSE
    /\ readyLock' = NoLock
    /\ singletonLock' = NoLock
    /\ candidateLock' = NoLock
    \* A crash before rename may retain the single fixed candidate name; it is
    \* never the ready path and is replaced by the next singleton owner.
    /\ UNCHANGED <<directorySafe, pathKind, pathPayload, candidatePresent,
                    candidatePayload, publishEpoch, restarts, announcements>>

CorruptOfflineMarker ==
    /\ phase \in {Offline, Exited}
    /\ readyLock = NoLock
    /\ pathKind' \in {Regular, Symlink, Other}
    /\ pathPayload' \in {Empty, Exact, Malformed}
    /\ UNCHANGED <<phase, directorySafe, initializationComplete, readyLock,
                    singletonLock, candidatePresent, candidatePayload,
                    candidateLock, publishEpoch, restarts, announcements>>

CorruptOfflineDirectory ==
    /\ phase \in {Offline, Exited}
    /\ directorySafe' = FALSE
    /\ readyLock' = NoLock
    /\ UNCHANGED <<phase, initializationComplete, pathKind, pathPayload,
                    singletonLock, candidatePresent, candidatePayload,
                    candidateLock, publishEpoch, restarts, announcements>>

RepairOfflineDirectory ==
    /\ phase \in {Offline, Exited}
    /\ ~directorySafe
    /\ directorySafe' = TRUE
    /\ UNCHANGED <<phase, initializationComplete, pathKind, pathPayload,
                    readyLock, singletonLock, candidatePresent,
                    candidatePayload, candidateLock, publishEpoch, restarts,
                    announcements>>

Announce ==
    /\ announcements < MaxAnnouncements
    /\ announcements' = announcements + 1
    /\ UNCHANGED <<phase, directorySafe, initializationComplete, pathKind,
                    pathPayload, readyLock, singletonLock, candidatePresent,
                    candidatePayload, candidateLock, publishEpoch, restarts>>

Next ==
    \/ Start
    \/ FinishInitialization
    \/ BeginPublication
    \/ WriteCandidate
    \/ AtomicInstall
    \/ RejectUnsafeDirectory
    \/ PublishFailure
    \/ Crash
    \/ CorruptOfflineMarker
    \/ CorruptOfflineDirectory
    \/ RepairOfflineDirectory
    \/ Announce

TypeOK ==
    /\ phase \in {Offline, Starting, Initialized, Publishing, Serving, Exited}
    /\ directorySafe \in BOOLEAN
    /\ initializationComplete \in BOOLEAN
    /\ pathKind \in {Absent, Regular, Symlink, Other}
    /\ pathPayload \in {Empty, Exact, Malformed}
    /\ readyLock \in {NoLock, AgentLock}
    /\ singletonLock \in {NoLock, AgentLock}
    /\ candidatePresent \in BOOLEAN
    /\ candidatePayload \in {Empty, Exact, Malformed}
    /\ candidateLock \in {NoLock, AgentLock}
    /\ publishEpoch \in 0..MaxRestarts
    /\ restarts \in 0..MaxRestarts
    /\ announcements \in 0..MaxAnnouncements

HealthRequiresLiveInitializedServer ==
    LocalHealthWouldSucceed =>
        /\ phase = Serving
        /\ AgentAlive
        /\ initializationComplete
        /\ singletonLock = AgentLock
        /\ publishEpoch > 0

DeadAgentCannotRemainHealthy ==
    ~AgentAlive => ~LocalHealthWouldSucceed

ServingOwnsExactReadyInode ==
    phase = Serving =>
        /\ directorySafe
        /\ pathKind = Regular
        /\ pathPayload = Exact
        /\ readyLock = AgentLock
        /\ singletonLock = AgentLock
        /\ ~candidatePresent

CandidateIsNeverReadiness ==
    candidatePresent => pathKind # Absent \/ ~LocalHealthWouldSucceed

PublicationIsAtomicAndComplete ==
    readyLock = AgentLock =>
        /\ phase = Serving
        /\ pathKind = Regular
        /\ pathPayload = Exact
        /\ publishEpoch > 0

CrashResidueIsBoundedAndUnowned ==
    candidateLock = AgentLock =>
        /\ phase = Publishing
        /\ singletonLock = AgentLock
        /\ candidatePresent

AnnounceCannotCreateAuthority ==
    publishEpoch = 0 => ~LocalHealthWouldSucceed

SettlePublication == WriteCandidate \/ AtomicInstall \/ PublishFailure \/ Crash

PublicationEventuallySettles ==
    phase = Publishing ~> phase # Publishing

Spec == Init /\ [][Next]_vars /\ WF_vars(SettlePublication)

=============================================================================
