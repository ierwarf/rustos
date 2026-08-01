----------------- MODULE AcceptanceProfilePublication -----------------
EXTENDS Naturals

(***************************************************************************
The private acceptance registry may appear after uiserver starts.  An initial
miss starts one bounded low-priority watcher; an exact contract published
before the deadline enables and announces profiling exactly once.
***************************************************************************)

CONSTANT MaxTicks
Start == "start"
Watching == "watching"
Enabled == "enabled"
Expired == "expired"

VARIABLES phase, tick, registryAvailable, watching, profileEnabled,
          announcements

vars == <<phase, tick, registryAvailable, watching, profileEnabled,
          announcements>>

Init ==
    /\ phase = Start /\ tick = 0 /\ registryAvailable = FALSE
    /\ watching = FALSE /\ profileEnabled = FALSE /\ announcements = 0

StartWatcherAfterInitialMiss ==
    /\ phase = Start /\ ~registryAvailable
    /\ phase' = Watching /\ watching' = TRUE
    /\ UNCHANGED <<tick, registryAvailable, profileEnabled, announcements>>

PublishExactContract ==
    /\ phase = Watching /\ tick < MaxTicks /\ ~registryAvailable
    /\ registryAvailable' = TRUE
    /\ UNCHANGED <<phase, tick, watching, profileEnabled, announcements>>

PollExactContract ==
    /\ phase = Watching /\ watching /\ registryAvailable
    /\ phase' = Enabled /\ watching' = FALSE /\ profileEnabled' = TRUE
    /\ announcements' = 1
    /\ UNCHANGED <<tick, registryAvailable>>

Tick ==
    /\ phase = Watching /\ watching /\ ~registryAvailable /\ tick < MaxTicks
    /\ tick' = tick + 1
    /\ UNCHANGED <<phase, registryAvailable, watching, profileEnabled,
                    announcements>>

Expire ==
    /\ phase = Watching /\ watching /\ ~registryAvailable /\ tick = MaxTicks
    /\ phase' = Expired /\ watching' = FALSE
    /\ UNCHANGED <<tick, registryAvailable, profileEnabled, announcements>>

Terminal ==
    /\ phase \in {Enabled, Expired}
    /\ UNCHANGED vars

Next == StartWatcherAfterInitialMiss \/ PublishExactContract
        \/ PollExactContract \/ Tick \/ Expire \/ Terminal

Spec == Init /\ [][Next]_vars /\ WF_vars(PollExactContract)

TypeOK ==
    /\ phase \in {Start, Watching, Enabled, Expired}
    /\ tick \in 0..MaxTicks /\ registryAvailable \in BOOLEAN
    /\ watching \in BOOLEAN /\ profileEnabled \in BOOLEAN
    /\ announcements \in 0..1

InitialMissRetainsBoundedObserver ==
    phase = Watching => watching /\ tick <= MaxTicks
EnabledIsAnnouncedExactlyOnce ==
    profileEnabled <=> (phase = Enabled /\ announcements = 1)
ExpiredObserverCannotEnable == phase = Expired => ~watching /\ ~profileEnabled

PublishedContractEventuallyEnables == registryAvailable ~> profileEnabled

=============================================================================
