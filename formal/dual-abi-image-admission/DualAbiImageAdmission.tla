------------------------ MODULE DualAbiImageAdmission ------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models the common ELF64/PE64 image-plan admission boundary implemented by
`libs/rustos-image-admission` and called by `services/loaderd/src/main.rs`.

The byte parsers remain format-specific and service-owned. After parsing, both
formats must cross the same finite gate before any process-broker mapping can
be committed: every region is bounded and non-overlapping, W^X holds, and a
main-image entry belongs to an executable region. Only an entryless PE DLL may
use entry zero. Rejected input never reaches the mapping transition.
*******************************************************************************)

CONSTANTS Formats, Regions, RegionFlagSets, MaxAddress, UserStart, UserEnd,
          MaxRegions

Addresses == 0..MaxAddress
NoFormat == "none"
Decisions == {"idle", "pending", "accepted", "rejected"}

VARIABLES format,
          present,
          regionStart,
          regionEnd,
          regionFlags,
          entryPoint,
          allowZeroEntry,
          decision,
          mapped

vars == <<format, present, regionStart, regionEnd, regionFlags, entryPoint,
          allowZeroEntry, decision, mapped>>

RegionsAreBounded ==
    /\ present # {}
    /\ Cardinality(present) <= MaxRegions
    /\ \A region \in present:
        /\ regionStart[region] >= UserStart
        /\ regionStart[region] < regionEnd[region]
        /\ regionEnd[region] <= UserEnd

RegionsDoNotOverlap ==
    \A left, right \in present:
        left # right =>
            \/ regionEnd[left] <= regionStart[right]
            \/ regionEnd[right] <= regionStart[left]

WritableXorExecutable ==
    \A region \in present:
        ~("W" \in regionFlags[region] /\ "X" \in regionFlags[region])

EntryIsExecutable ==
    \/ /\ entryPoint = 0
       /\ allowZeroEntry
       /\ format = "PE64"
    \/ /\ entryPoint # 0
       /\ \E region \in present:
            /\ "X" \in regionFlags[region]
            /\ regionStart[region] <= entryPoint
            /\ entryPoint < regionEnd[region]

ValidImagePlan ==
    /\ format \in Formats
    /\ allowZeroEntry => format = "PE64"
    /\ RegionsAreBounded
    /\ RegionsDoNotOverlap
    /\ WritableXorExecutable
    /\ EntryIsExecutable

Init ==
    /\ format = NoFormat
    /\ present = {}
    /\ regionStart = [region \in Regions |-> 0]
    /\ regionEnd = [region \in Regions |-> 0]
    /\ regionFlags = [region \in Regions |-> {}]
    /\ entryPoint = 0
    /\ allowZeroEntry = FALSE
    /\ decision = "idle"
    /\ mapped = FALSE

SubmitImage(chosenFormat, chosenPresent, chosenStart, chosenEnd, chosenFlags,
            chosenEntry, chosenAllowZero) ==
    /\ decision = "idle"
    /\ chosenFormat \in Formats
    /\ chosenPresent \in SUBSET Regions
    /\ chosenStart \in [Regions -> Addresses]
    /\ chosenEnd \in [Regions -> Addresses]
    /\ chosenFlags \in [Regions -> RegionFlagSets]
    /\ chosenEntry \in Addresses
    /\ chosenAllowZero \in BOOLEAN
    /\ format' = chosenFormat
    /\ present' = chosenPresent
    /\ regionStart' = chosenStart
    /\ regionEnd' = chosenEnd
    /\ regionFlags' = chosenFlags
    /\ entryPoint' = chosenEntry
    /\ allowZeroEntry' = chosenAllowZero
    /\ decision' = "pending"
    /\ mapped' = FALSE

Validate ==
    /\ decision = "pending"
    /\ decision' = IF ValidImagePlan THEN "accepted" ELSE "rejected"
    /\ UNCHANGED <<format, present, regionStart, regionEnd, regionFlags,
                    entryPoint, allowZeroEntry, mapped>>

MapAccepted ==
    /\ decision = "accepted"
    /\ ~mapped
    /\ mapped' = TRUE
    /\ UNCHANGED <<format, present, regionStart, regionEnd, regionFlags,
                    entryPoint, allowZeroEntry, decision>>

Next ==
    \/ /\ decision = "idle"
       /\ \E chosenFormat \in Formats,
             chosenPresent \in SUBSET Regions,
             chosenStart \in [Regions -> Addresses],
             chosenEnd \in [Regions -> Addresses],
             chosenFlags \in [Regions -> RegionFlagSets],
             chosenEntry \in Addresses,
             chosenAllowZero \in BOOLEAN:
             SubmitImage(chosenFormat, chosenPresent, chosenStart, chosenEnd,
                         chosenFlags, chosenEntry, chosenAllowZero)
    \/ Validate
    \/ MapAccepted

Spec ==
    Init /\ [][Next]_vars /\ WF_vars(Validate) /\ WF_vars(MapAccepted)

TypeOK ==
    /\ format \in Formats \cup {NoFormat}
    /\ present \in SUBSET Regions
    /\ regionStart \in [Regions -> Addresses]
    /\ regionEnd \in [Regions -> Addresses]
    /\ regionFlags \in [Regions -> RegionFlagSets]
    /\ entryPoint \in Addresses
    /\ allowZeroEntry \in BOOLEAN
    /\ decision \in Decisions
    /\ mapped \in BOOLEAN

AcceptedPlanSatisfiesCommonGate == decision = "accepted" => ValidImagePlan
MappedPlanWasAccepted == mapped => decision = "accepted"
RejectedPlanNeverMaps == decision = "rejected" => ~mapped
PendingEventuallySettles == [] (decision = "pending" => <> (decision # "pending"))
AcceptedEventuallyMaps == [] (decision = "accepted" => <> mapped)

=============================================================================
