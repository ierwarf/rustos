----------------------------- MODULE DvmReadCache -----------------------------
EXTENDS Naturals

(*******************************************************************************
Models the storaged read-ahead cache as a composition boundary between
generation-bound DVM transport and vfsd filesystem reads.

Only an exact live generation may hit. A cache miss arms a bounded multi-ticket
read-ahead batch and publishes no window until every completion succeeds.
A failed completion clears cached authority. A fresh fill replaces a different
epoch, overlapping windows do not coexist, the cache remains bounded, and write
or restart clears all cached authority before completing.

Concrete owner:
  * services/storaged/src/block.rs
*******************************************************************************)

CONSTANT MaxCacheWindows

Scenarios == {"read", "read-fail", "write", "restart"}
TerminalPhases ==
    {"cache-served", "fresh-served", "fill-failed",
     "stale", "mutated", "restarted"}

VARIABLES phase, scenario, liveGeneration, requestGeneration,
          cacheGeneration, cacheWindows, rangeCovered, fillWindows,
          sequentialMiss

vars ==
    <<phase, scenario, liveGeneration, requestGeneration,
      cacheGeneration, cacheWindows, rangeCovered, fillWindows,
      sequentialMiss>>

Init ==
    /\ phase = "idle"
    /\ scenario \in Scenarios
    /\ liveGeneration = 1
    /\ requestGeneration \in 1..2
    /\ cacheWindows \in 0..MaxCacheWindows
    /\ cacheGeneration = IF cacheWindows = 0 THEN 0 ELSE liveGeneration
    /\ rangeCovered \in BOOLEAN
    /\ fillWindows = 0
    /\ sequentialMiss \in BOOLEAN

ServeCache ==
    /\ phase = "idle"
    /\ scenario \in {"read", "read-fail"}
    /\ requestGeneration = liveGeneration
    /\ cacheWindows > 0
    /\ cacheGeneration = liveGeneration
    /\ rangeCovered
    /\ phase' = "cache-served"
    /\ UNCHANGED
        <<scenario, liveGeneration, requestGeneration,
          cacheGeneration, cacheWindows, rangeCovered, fillWindows,
          sequentialMiss>>

BeginFill ==
    /\ phase = "idle"
    /\ scenario \in {"read", "read-fail"}
    /\ requestGeneration = liveGeneration
    /\ (cacheWindows = 0 \/ ~rangeCovered)
    /\ fillWindows' \in
        IF sequentialMiss THEN 1..MaxCacheWindows ELSE {1}
    /\ phase' = "filling"
    /\ UNCHANGED
        <<scenario, liveGeneration, requestGeneration,
          cacheGeneration, cacheWindows, rangeCovered, sequentialMiss>>

FillFresh ==
    /\ phase = "filling"
    /\ scenario = "read"
    /\ phase' = "fresh-served"
    /\ cacheGeneration' = liveGeneration
    /\ cacheWindows' = fillWindows
    /\ UNCHANGED
        <<scenario, liveGeneration, requestGeneration,
          rangeCovered, fillWindows, sequentialMiss>>

FailFill ==
    /\ phase = "filling"
    /\ scenario = "read-fail"
    /\ phase' = "fill-failed"
    /\ cacheGeneration' = 0
    /\ cacheWindows' = 0
    /\ UNCHANGED
        <<scenario, liveGeneration, requestGeneration,
          rangeCovered, fillWindows, sequentialMiss>>

RejectStale ==
    /\ phase = "idle"
    /\ scenario \in {"read", "read-fail"}
    /\ requestGeneration # liveGeneration
    /\ phase' = "stale"
    /\ UNCHANGED
        <<scenario, liveGeneration, requestGeneration,
          cacheGeneration, cacheWindows, rangeCovered, fillWindows,
          sequentialMiss>>

Write ==
    /\ phase = "idle"
    /\ scenario = "write"
    /\ phase' = "mutated"
    /\ cacheGeneration' = 0
    /\ cacheWindows' = 0
    /\ UNCHANGED
        <<scenario, liveGeneration, requestGeneration, rangeCovered, fillWindows,
          sequentialMiss>>

Restart ==
    /\ phase = "idle"
    /\ scenario = "restart"
    /\ phase' = "restarted"
    /\ liveGeneration' = liveGeneration + 1
    /\ cacheGeneration' = 0
    /\ cacheWindows' = 0
    /\ UNCHANGED
        <<scenario, requestGeneration, rangeCovered, fillWindows,
          sequentialMiss>>

Next ==
    \/ ServeCache
    \/ BeginFill
    \/ FillFresh
    \/ FailFill
    \/ RejectStale
    \/ Write
    \/ Restart

TypeOK ==
    /\ phase \in {"idle", "filling"} \cup TerminalPhases
    /\ scenario \in Scenarios
    /\ liveGeneration \in 1..2
    /\ requestGeneration \in 1..2
    /\ cacheGeneration \in 0..2
    /\ cacheWindows \in 0..MaxCacheWindows
    /\ rangeCovered \in BOOLEAN
    /\ fillWindows \in 0..MaxCacheWindows
    /\ sequentialMiss \in BOOLEAN

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
    phase \in {"fill-failed", "mutated", "restarted"} =>
        /\ cacheWindows = 0
        /\ cacheGeneration = 0

FreshFillRequiresBoundedPipeline ==
    phase = "fresh-served" =>
        /\ fillWindows > 0
        /\ fillWindows <= MaxCacheWindows
        /\ cacheWindows = fillWindows

NonSequentialFillIsOneWindow ==
    phase = "fresh-served" /\ ~sequentialMiss =>
        fillWindows = 1

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

EventuallyTerminal == <> (phase \in TerminalPhases)
=============================================================================
