------------------- MODULE ProcessLifecycleTransaction -------------------
EXTENDS Naturals

(***************************************************************************
Spawn, exec, exit, and reap use one non-reusable transaction namespace while
process-slot and MM generations remain separate. Partial frame acquisition is
unpublished and must roll back exactly; published frames remain owned until
all tasks/references/reply/fault/TLB authorities reach zero and reap commits.
***************************************************************************)

CONSTANT MaxTxn, TotalFrames

Idle == "idle"
SpawnReserved == "spawn-reserved"
SpawnStaging == "spawn-staging"
Running == "running"
ExecReserved == "exec-reserved"
ExecStaging == "exec-staging"
ExitPending == "exit-pending"
Exiting == "exiting"
ReapQueued == "reap-queued"
Dead == "dead"

VARIABLES phase, processGeneration, mmGeneration, nextTxn, activeTxn,
          tokenLive, unpublishedFrames, publishedFrames, freeFrames,
          tasks, references, replyCaps, faultTokens, tlbTargets,
          attachOpen, oldBundleLive, exitRequested, rollbackCount,
          completedTxn

vars == <<phase, processGeneration, mmGeneration, nextTxn, activeTxn,
          tokenLive, unpublishedFrames, publishedFrames, freeFrames,
          tasks, references, replyCaps, faultTokens, tlbTargets,
          attachOpen, oldBundleLive, exitRequested, rollbackCount,
          completedTxn>>

CanAllocateTxn == nextTxn \in 1..MaxTxn

Init ==
    /\ phase = Idle
    /\ processGeneration = 1
    /\ mmGeneration = 0
    /\ nextTxn = 1
    /\ activeTxn = 0
    /\ tokenLive = FALSE
    /\ unpublishedFrames = 0
    /\ publishedFrames = 0
    /\ freeFrames = TotalFrames
    /\ tasks = 0
    /\ references = 0
    /\ replyCaps = 0
    /\ faultTokens = 0
    /\ tlbTargets = 0
    /\ attachOpen = FALSE
    /\ oldBundleLive = FALSE
    /\ exitRequested = FALSE
    /\ rollbackCount = 0
    /\ completedTxn = 0

ReserveSpawn ==
    /\ phase = Idle
    /\ CanAllocateTxn
    /\ phase' = SpawnReserved
    /\ activeTxn' = nextTxn
    /\ nextTxn' = nextTxn + 1
    /\ tokenLive' = TRUE
    /\ UNCHANGED <<processGeneration, mmGeneration, unpublishedFrames,
                    publishedFrames, freeFrames, tasks, references, replyCaps,
                    faultTokens, tlbTargets, attachOpen, oldBundleLive,
                    exitRequested, rollbackCount, completedTxn>>

AcquireSpawnFrame ==
    /\ phase \in {SpawnReserved, SpawnStaging}
    /\ tokenLive
    /\ freeFrames > 0
    /\ phase' = SpawnStaging
    /\ unpublishedFrames' = unpublishedFrames + 1
    /\ freeFrames' = freeFrames - 1
    /\ UNCHANGED <<processGeneration, mmGeneration, nextTxn, activeTxn,
                    tokenLive, publishedFrames, tasks, references, replyCaps,
                    faultTokens, tlbTargets, attachOpen, oldBundleLive,
                    exitRequested, rollbackCount, completedTxn>>

PublishSpawn ==
    /\ phase \in {SpawnReserved, SpawnStaging}
    /\ tokenLive
    /\ unpublishedFrames > 0
    /\ phase' = Running
    /\ mmGeneration' = 1
    /\ publishedFrames' = unpublishedFrames
    /\ unpublishedFrames' = 0
    /\ tasks' = 1
    /\ references' = 1
    /\ attachOpen' = TRUE
    /\ oldBundleLive' = TRUE
    /\ tokenLive' = FALSE
    /\ completedTxn' = activeTxn
    /\ UNCHANGED <<processGeneration, nextTxn, activeTxn, freeFrames,
                    replyCaps, faultTokens, tlbTargets, exitRequested,
                    rollbackCount>>

AbortSpawn ==
    /\ phase \in {SpawnReserved, SpawnStaging}
    /\ tokenLive
    /\ phase' = Idle
    /\ freeFrames' = freeFrames + unpublishedFrames
    /\ rollbackCount' = rollbackCount + unpublishedFrames
    /\ unpublishedFrames' = 0
    /\ tokenLive' = FALSE
    /\ completedTxn' = activeTxn
    /\ activeTxn' = 0
    /\ UNCHANGED <<processGeneration, mmGeneration, nextTxn, publishedFrames,
                    tasks, references, replyCaps, faultTokens, tlbTargets,
                    attachOpen, oldBundleLive, exitRequested>>

AttachThread ==
    /\ phase = Running
    /\ attachOpen
    /\ tasks < 2
    /\ tasks' = tasks + 1
    /\ references' = references + 1
    /\ UNCHANGED <<phase, processGeneration, mmGeneration, nextTxn,
                    activeTxn, tokenLive, unpublishedFrames, publishedFrames,
                    freeFrames, replyCaps, faultTokens, tlbTargets, attachOpen,
                    oldBundleLive, exitRequested, rollbackCount, completedTxn>>

DetachThread ==
    /\ phase \in {Running, Exiting}
    /\ tasks > 0
    /\ tasks' = tasks - 1
    /\ references' = references - 1
    /\ UNCHANGED <<phase, processGeneration, mmGeneration, nextTxn,
                    activeTxn, tokenLive, unpublishedFrames, publishedFrames,
                    freeFrames, replyCaps, faultTokens, tlbTargets, attachOpen,
                    oldBundleLive, exitRequested, rollbackCount, completedTxn>>

ReserveExec ==
    /\ phase = Running
    /\ tasks = 1
    /\ CanAllocateTxn
    /\ phase' = ExecReserved
    /\ activeTxn' = nextTxn
    /\ nextTxn' = nextTxn + 1
    /\ tokenLive' = TRUE
    /\ attachOpen' = FALSE
    /\ exitRequested' = FALSE
    /\ UNCHANGED <<processGeneration, mmGeneration, unpublishedFrames,
                    publishedFrames, freeFrames, tasks, references, replyCaps,
                    faultTokens, tlbTargets, oldBundleLive, rollbackCount,
                    completedTxn>>

AcquireExecFrame ==
    /\ phase \in {ExecReserved, ExecStaging}
    /\ tokenLive
    /\ freeFrames > 0
    /\ phase' = ExecStaging
    /\ unpublishedFrames' = unpublishedFrames + 1
    /\ freeFrames' = freeFrames - 1
    /\ UNCHANGED <<processGeneration, mmGeneration, nextTxn, activeTxn,
                    tokenLive, publishedFrames, tasks, references, replyCaps,
                    faultTokens, tlbTargets, attachOpen, oldBundleLive,
                    exitRequested, rollbackCount, completedTxn>>

AbortExec ==
    /\ phase \in {ExecReserved, ExecStaging}
    /\ tokenLive
    /\ ~exitRequested
    /\ phase' = Running
    /\ freeFrames' = freeFrames + unpublishedFrames
    /\ rollbackCount' = rollbackCount + unpublishedFrames
    /\ unpublishedFrames' = 0
    /\ tokenLive' = FALSE
    /\ attachOpen' = TRUE
    /\ completedTxn' = activeTxn
    /\ UNCHANGED <<processGeneration, mmGeneration, nextTxn, activeTxn,
                    publishedFrames, tasks, references, replyCaps, faultTokens,
                    tlbTargets, oldBundleLive, exitRequested>>

PublishExec ==
    /\ phase \in {ExecReserved, ExecStaging, ExitPending}
    /\ tokenLive
    /\ unpublishedFrames > 0
    /\ phase' = IF exitRequested THEN Exiting ELSE Running
    /\ mmGeneration' = mmGeneration + 1
    /\ freeFrames' = freeFrames + publishedFrames
    /\ publishedFrames' = unpublishedFrames
    /\ unpublishedFrames' = 0
    /\ tokenLive' = exitRequested
    /\ attachOpen' = ~exitRequested
    /\ oldBundleLive' = TRUE
    /\ completedTxn' = IF exitRequested THEN completedTxn ELSE activeTxn
    /\ UNCHANGED <<processGeneration, nextTxn, activeTxn, tasks, references,
                    replyCaps, faultTokens, tlbTargets, exitRequested,
                    rollbackCount>>

RequestExitDuringExec ==
    /\ phase \in {ExecReserved, ExecStaging}
    /\ ~exitRequested
    /\ CanAllocateTxn
    /\ phase' = ExitPending
    /\ exitRequested' = TRUE
    /\ attachOpen' = FALSE
    /\ completedTxn' = activeTxn
    /\ activeTxn' = nextTxn
    /\ nextTxn' = nextTxn + 1
    /\ UNCHANGED <<processGeneration, mmGeneration, tokenLive,
                    unpublishedFrames, publishedFrames, freeFrames,
                    tasks, references, replyCaps, faultTokens, tlbTargets,
                    oldBundleLive, rollbackCount>>

CancelExecForExit ==
    /\ phase = ExitPending
    /\ tokenLive
    /\ phase' = Exiting
    /\ freeFrames' = freeFrames + unpublishedFrames
    /\ rollbackCount' = rollbackCount + unpublishedFrames
    /\ unpublishedFrames' = 0
    /\ UNCHANGED <<processGeneration, mmGeneration, nextTxn, activeTxn,
                    tokenLive, publishedFrames, tasks, references, replyCaps,
                    faultTokens, tlbTargets, attachOpen, oldBundleLive,
                    exitRequested, completedTxn>>

BeginExit ==
    /\ phase = Running
    /\ CanAllocateTxn
    /\ phase' = Exiting
    /\ activeTxn' = nextTxn
    /\ nextTxn' = nextTxn + 1
    /\ tokenLive' = TRUE
    /\ attachOpen' = FALSE
    /\ exitRequested' = TRUE
    /\ UNCHANGED <<processGeneration, mmGeneration, unpublishedFrames,
                    publishedFrames, freeFrames, tasks, references, replyCaps,
                    faultTokens, tlbTargets, oldBundleLive, rollbackCount,
                    completedTxn>>

PublishAuthority ==
    /\ phase = Running
    /\ replyCaps + faultTokens + tlbTargets = 0
    /\ \/ /\ replyCaps' = 1
           /\ UNCHANGED <<faultTokens, tlbTargets>>
       \/ /\ faultTokens' = 1
           /\ UNCHANGED <<replyCaps, tlbTargets>>
       \/ /\ tlbTargets' = 1
           /\ UNCHANGED <<replyCaps, faultTokens>>
    /\ UNCHANGED <<phase, processGeneration, mmGeneration, nextTxn,
                    activeTxn, tokenLive, unpublishedFrames, publishedFrames,
                    freeFrames, tasks, references, attachOpen, oldBundleLive,
                    exitRequested, rollbackCount, completedTxn>>

SettleAuthority ==
    /\ phase = Exiting
    /\ replyCaps + faultTokens + tlbTargets > 0
    /\ \/ /\ replyCaps > 0
           /\ replyCaps' = replyCaps - 1
           /\ UNCHANGED <<faultTokens, tlbTargets>>
       \/ /\ replyCaps = 0
           /\ faultTokens > 0
           /\ faultTokens' = faultTokens - 1
           /\ UNCHANGED <<replyCaps, tlbTargets>>
       \/ /\ replyCaps = 0
           /\ faultTokens = 0
           /\ tlbTargets > 0
           /\ tlbTargets' = tlbTargets - 1
           /\ UNCHANGED <<replyCaps, faultTokens>>
    /\ UNCHANGED <<phase, processGeneration, mmGeneration, nextTxn,
                    activeTxn, tokenLive, unpublishedFrames, publishedFrames,
                    freeFrames, tasks, references, attachOpen, oldBundleLive,
                    exitRequested, rollbackCount, completedTxn>>

QueueReap ==
    /\ phase = Exiting
    /\ tasks = 0
    /\ references = 0
    /\ replyCaps = 0
    /\ faultTokens = 0
    /\ tlbTargets = 0
    /\ phase' = ReapQueued
    /\ UNCHANGED <<processGeneration, mmGeneration, nextTxn, activeTxn,
                    tokenLive, unpublishedFrames, publishedFrames, freeFrames,
                    tasks, references, replyCaps, faultTokens, tlbTargets,
                    attachOpen, oldBundleLive, exitRequested, rollbackCount,
                    completedTxn>>

Reap ==
    /\ phase = ReapQueued
    /\ phase' = Dead
    /\ freeFrames' = freeFrames + publishedFrames
    /\ publishedFrames' = 0
    /\ oldBundleLive' = FALSE
    /\ tokenLive' = FALSE
    /\ completedTxn' = activeTxn
    /\ UNCHANGED <<processGeneration, mmGeneration, nextTxn, activeTxn,
                    unpublishedFrames, tasks, references, replyCaps,
                    faultTokens, tlbTargets, attachOpen, exitRequested,
                    rollbackCount>>

ReuseSlot ==
    /\ phase = Dead
    /\ processGeneration < 2
    /\ phase' = Idle
    /\ processGeneration' = processGeneration + 1
    /\ mmGeneration' = 0
    /\ activeTxn' = 0
    /\ exitRequested' = FALSE
    /\ UNCHANGED <<nextTxn, tokenLive, unpublishedFrames, publishedFrames,
                    freeFrames, tasks, references, replyCaps, faultTokens,
                    tlbTargets, attachOpen, oldBundleLive, rollbackCount,
                    completedTxn>>

Terminal ==
    /\ \/ phase = Dead
       \/ /\ phase = Idle
           /\ ~CanAllocateTxn
       \/ /\ phase = Running
           /\ ~CanAllocateTxn
    /\ UNCHANGED vars

Next == ReserveSpawn \/ AcquireSpawnFrame \/ PublishSpawn \/ AbortSpawn
        \/ AttachThread \/ DetachThread \/ ReserveExec \/ AcquireExecFrame
        \/ AbortExec \/ PublishExec \/ RequestExitDuringExec \/ CancelExecForExit \/ BeginExit
        \/ PublishAuthority
        \/ SettleAuthority \/ QueueReap \/ Reap \/ ReuseSlot \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {Idle, SpawnReserved, SpawnStaging, Running, ExecReserved,
                   ExecStaging, ExitPending, Exiting, ReapQueued, Dead}
    /\ processGeneration \in 1..2
    /\ mmGeneration \in 0..MaxTxn
    /\ nextTxn \in 1..(MaxTxn + 1)
    /\ activeTxn \in 0..MaxTxn
    /\ completedTxn \in 0..MaxTxn
    /\ tokenLive \in BOOLEAN
    /\ unpublishedFrames \in 0..TotalFrames
    /\ publishedFrames \in 0..TotalFrames
    /\ freeFrames \in 0..TotalFrames
    /\ tasks \in 0..2
    /\ references \in 0..2
    /\ replyCaps \in 0..1
    /\ faultTokens \in 0..1
    /\ tlbTargets \in 0..1
    /\ attachOpen \in BOOLEAN
    /\ oldBundleLive \in BOOLEAN
    /\ exitRequested \in BOOLEAN
    /\ rollbackCount \in 0..(MaxTxn * TotalFrames)

FrameConservation ==
    freeFrames + unpublishedFrames + publishedFrames = TotalFrames

NoEarlyReclaim ==
    publishedFrames = 0 => phase \in {Idle, SpawnReserved, SpawnStaging, Dead}

ReapRequiresNoAuthority ==
    phase \in {ReapQueued, Dead} =>
        /\ tasks = 0
        /\ references = 0
        /\ replyCaps = 0
        /\ faultTokens = 0
        /\ tlbTargets = 0

AttachmentSealedOutsideRunning == attachOpen => phase = Running
LiveTokenOwnsExactTransaction == tokenLive => activeTxn > completedTxn
GenerationNeverAliases == activeTxn = 0 \/ activeTxn < nextTxn
ExecPublicationIsWhole == mmGeneration > 0 => oldBundleLive \/ phase = Dead

=============================================================================
