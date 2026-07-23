----------------------------- MODULE DvmReadCache -----------------------------
EXTENDS Naturals

(*******************************************************************************
Models the storaged read-ahead cache as a composition boundary between
generation-bound DVM transport and vfsd filesystem reads.

Only an exact live generation may hit.  A fresh fill replaces a different
epoch, overlapping windows do not coexist, the cache remains bounded, and
write or restart clears all cached authority before completing.

Concrete owner:
  * services/storaged/src/block.rs
*******************************************************************************)

CONSTANT MaxCacheWindows

Scenarios == {"read", "write", "restart"}
TerminalPhases == {"cache-served", "fresh-served", "stale", "mutated", "restarted"}

VARIABLES phase, scenario, liveGeneration, requestGeneration,
          cacheGeneration, cacheWindows, rangeCovered

vars ==
    <<phase, scenario, liveGeneration, requestGeneration,
      cacheGeneration, cacheWindows, rangeCovered>>

Init ==
    /\ phase = "idle"
    /\ scenario \in Scenarios
    /\ liveGeneration = 1
    /\ requestGeneration \in 1..2
    /\ cacheWindows \in 0..MaxCacheWindows
    /\ cacheGeneration = IF cacheWindows = 0 THEN 0 ELSE liveGeneration
    /\ rangeCovered \in BOOLEAN

ServeCache ==
    /\ phase = "idle"
    /\ scenario = "read"
    /\ requestGeneration = liveGeneration
    /\ cacheWindows > 0
    /\ cacheGeneration = liveGeneration
    /\ rangeCovered
    /\ phase' = "cache-served"
    /\ UNCHANGED
        <<scenario, liveGeneration, requestGeneration,
          cacheGeneration, cacheWindows, rangeCovered>>

FillFresh ==
    /\ phase = "idle"
    /\ scenario = "read"
    /\ requestGeneration = liveGeneration
    /\ (cacheWindows = 0 \/ ~rangeCovered)
    /\ phase' = "fresh-served"
    /\ cacheGeneration' = liveGeneration
    /\ cacheWindows' =
        IF cacheWindows = MaxCacheWindows
        THEN MaxCacheWindows
        ELSE cacheWindows + 1
    /\ UNCHANGED
        <<scenario, liveGeneration, requestGeneration, rangeCovered>>

RejectStale ==
    /\ phase = "idle"
    /\ scenario = "read"
    /\ requestGeneration # liveGeneration
    /\ phase' = "stale"
    /\ UNCHANGED
        <<scenario, liveGeneration, requestGeneration,
          cacheGeneration, cacheWindows, rangeCovered>>

Write ==
    /\ phase = "idle"
    /\ scenario = "write"
    /\ phase' = "mutated"
    /\ cacheGeneration' = 0
    /\ cacheWindows' = 0
    /\ UNCHANGED
        <<scenario, liveGeneration, requestGeneration, rangeCovered>>

Restart ==
    /\ phase = "idle"
    /\ scenario = "restart"
    /\ phase' = "restarted"
    /\ liveGeneration' = liveGeneration + 1
    /\ cacheGeneration' = 0
    /\ cacheWindows' = 0
    /\ UNCHANGED <<scenario, requestGeneration, rangeCovered>>

Next ==
    \/ ServeCache
    \/ FillFresh
    \/ RejectStale
    \/ Write
    \/ Restart

TypeOK ==
    /\ phase \in {"idle"} \cup TerminalPhases
    /\ scenario \in Scenarios
    /\ liveGeneration \in 1..2
    /\ requestGeneration \in 1..2
    /\ cacheGeneration \in 0..2
    /\ cacheWindows \in 0..MaxCacheWindows
    /\ rangeCovered \in BOOLEAN

CacheIsBounded ==
    cacheWindows <= MaxCacheWindows

CacheEpochMatchesLiveGeneration ==
    cacheWindows > 0 => cacheGeneration = liveGeneration

EmptyCacheCarriesNoEpoch ==
    cacheWindows = 0 => cacheGeneration = 0

StaleRequestNeverServes ==
    phase \in {"cache-served", "fresh-served"} =>
        requestGeneration = liveGeneration

CacheHitRequiresExactEpoch ==
    phase = "cache-served" =>
        /\ cacheWindows > 0
        /\ cacheGeneration = liveGeneration

MutationAndRestartClearCache ==
    phase \in {"mutated", "restarted"} =>
        /\ cacheWindows = 0
        /\ cacheGeneration = 0

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

EventuallyTerminal == <> (phase \in TerminalPhases)
=============================================================================
