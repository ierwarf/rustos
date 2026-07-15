------------------------- MODULE DualAbiByteParser --------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Owner: loaderd and libs/rustos-image-admission.
Linearization point: Admit, after every bounded raw-byte parsing stage and
before the first process-broker mapping. Rejected or mutated inputs never map.
*******************************************************************************)

CONSTANTS Formats, MaxRegions, MaxImports

VARIABLES format, headerValid, tableValid, regionCount, relocRequired,
          relocValid, importCount, importsValid, snapshotStable, decision,
          mapped

vars == <<format, headerValid, tableValid, regionCount, relocRequired,
          relocValid, importCount, importsValid, snapshotStable, decision,
          mapped>>

NoFormat == "none"
Decisions == {"idle", "parsed", "accepted", "rejected"}

ValidBytes ==
    /\ format \in Formats
    /\ headerValid
    /\ tableValid
    /\ regionCount \in 1..MaxRegions
    /\ (~relocRequired \/ relocValid)
    /\ importCount \in 0..MaxImports
    /\ importsValid
    /\ snapshotStable

Init ==
    /\ format = NoFormat
    /\ headerValid = FALSE
    /\ tableValid = FALSE
    /\ regionCount = 0
    /\ relocRequired = FALSE
    /\ relocValid = FALSE
    /\ importCount = 0
    /\ importsValid = FALSE
    /\ snapshotStable = FALSE
    /\ decision = "idle"
    /\ mapped = FALSE

Parse(f, hv, tv, rc, rr, rv, ic, iv, stable) ==
    /\ decision = "idle"
    /\ f \in Formats
    /\ rc \in 0..(MaxRegions + 1)
    /\ ic \in 0..(MaxImports + 1)
    /\ format' = f
    /\ headerValid' = hv
    /\ tableValid' = tv
    /\ regionCount' = rc
    /\ relocRequired' = rr
    /\ relocValid' = rv
    /\ importCount' = ic
    /\ importsValid' = iv
    /\ snapshotStable' = stable
    /\ decision' = "parsed"
    /\ mapped' = FALSE

Admit ==
    /\ decision = "parsed"
    /\ decision' = IF ValidBytes THEN "accepted" ELSE "rejected"
    /\ UNCHANGED <<format, headerValid, tableValid, regionCount,
                    relocRequired, relocValid, importCount, importsValid,
                    snapshotStable, mapped>>

MutateBeforeMap ==
    /\ decision = "accepted"
    /\ snapshotStable
    /\ ~mapped
    /\ snapshotStable' = FALSE
    /\ decision' = "rejected"
    /\ UNCHANGED <<format, headerValid, tableValid, regionCount,
                    relocRequired, relocValid, importCount, importsValid,
                    mapped>>

Map ==
    /\ decision = "accepted"
    /\ snapshotStable
    /\ mapped' = TRUE
    /\ UNCHANGED <<format, headerValid, tableValid, regionCount,
                    relocRequired, relocValid, importCount, importsValid,
                    snapshotStable, decision>>

Next ==
    \/ /\ decision = "idle"
       /\ \E f \in Formats, hv \in BOOLEAN, tv \in BOOLEAN,
             rc \in 0..(MaxRegions + 1), rr \in BOOLEAN, rv \in BOOLEAN,
             ic \in 0..(MaxImports + 1), iv \in BOOLEAN, stable \in BOOLEAN:
             Parse(f, hv, tv, rc, rr, rv, ic, iv, stable)
    \/ Admit
    \/ MutateBeforeMap
    \/ Map

Spec == Init /\ [][Next]_vars /\ WF_vars(Admit) /\ WF_vars(Map)

TypeOK ==
    /\ format \in Formats \cup {NoFormat}
    /\ headerValid \in BOOLEAN
    /\ tableValid \in BOOLEAN
    /\ regionCount \in 0..(MaxRegions + 1)
    /\ relocRequired \in BOOLEAN
    /\ relocValid \in BOOLEAN
    /\ importCount \in 0..(MaxImports + 1)
    /\ importsValid \in BOOLEAN
    /\ snapshotStable \in BOOLEAN
    /\ decision \in Decisions
    /\ mapped \in BOOLEAN

AcceptedBytesAreValid == decision = "accepted" => ValidBytes
MappedBytesWereAccepted == mapped => decision = "accepted" /\ ValidBytes
RejectedBytesNeverMap == decision = "rejected" => ~mapped
ParsedEventuallySettles == [] (decision = "parsed" => <> (decision # "parsed"))

=============================================================================
