-------------------- MODULE KernelResourceAccounting --------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Owner: kernel IPC runtime and process table.

User processes may allocate endpoints, shared regions, and threads only within
both object-specific and owner-specific ceilings. Endpoint and thread release
returns quota at their terminal transition. A dropped shared region remains
charged to its exact process while backing is queued for physical reclaim; the
charge is returned only by the reclaim transition. Process exit revokes
endpoints and threads immediately but merely queues live regions for reclaim.
All admission transitions are allocation-free with respect to the quota
ledger, so a rejected owner cannot partially publish an object.
***************************************************************************)

CONSTANTS Processes, EndpointIds, RegionIds, RegionSizes,
          MaxEndpointsPerProcess, MaxRegionsPerProcess,
          MaxRegionBytesPerProcess, MaxGlobalRegionBytes,
          MaxThreadsPerProcess

NoProcess == "none"
Free == "free"
Live == "live"
Reclaim == "reclaim"

VARIABLES alive, endpointOwner, regionOwner, regionState, regionBytes, threadCount
vars == <<alive, endpointOwner, regionOwner, regionState, regionBytes, threadCount>>

EndpointCount(p) == Cardinality({e \in EndpointIds: endpointOwner[e] = p})
RECURSIVE SumRegions(_)
SumRegions(regions) ==
    IF regions = {} THEN 0
    ELSE LET region == CHOOSE r \in regions: TRUE
         IN regionBytes[region] + SumRegions(regions \ {region})
RegionCount(p) ==
    Cardinality({r \in RegionIds:
        regionOwner[r] = p /\ regionState[r] \in {Live, Reclaim}})
RegionCharge(p) ==
    LET owned == {r \in RegionIds:
        regionOwner[r] = p /\ regionState[r] \in {Live, Reclaim}}
    IN  SumRegions(owned)
GlobalRegionCharge ==
    LET charged == {r \in RegionIds: regionState[r] \in {Live, Reclaim}}
    IN  SumRegions(charged)

Init ==
    /\ alive = Processes
    /\ endpointOwner = [e \in EndpointIds |-> NoProcess]
    /\ regionOwner = [r \in RegionIds |-> NoProcess]
    /\ regionState = [r \in RegionIds |-> Free]
    /\ regionBytes = [r \in RegionIds |-> 0]
    /\ threadCount = [p \in Processes |-> 0]

CreateEndpoint(p, e) ==
    /\ p \in alive
    /\ endpointOwner[e] = NoProcess
    /\ EndpointCount(p) < MaxEndpointsPerProcess
    /\ endpointOwner' = [endpointOwner EXCEPT ![e] = p]
    /\ UNCHANGED <<alive, regionOwner, regionState, regionBytes, threadCount>>

CloseEndpoint(e) ==
    /\ endpointOwner[e] # NoProcess
    /\ endpointOwner' = [endpointOwner EXCEPT ![e] = NoProcess]
    /\ UNCHANGED <<alive, regionOwner, regionState, regionBytes, threadCount>>

CreateRegion(p, r, bytes) ==
    /\ p \in alive
    /\ regionState[r] = Free
    /\ bytes \in RegionSizes
    /\ RegionCount(p) < MaxRegionsPerProcess
    /\ RegionCharge(p) + bytes <= MaxRegionBytesPerProcess
    /\ GlobalRegionCharge + bytes <= MaxGlobalRegionBytes
    /\ regionOwner' = [regionOwner EXCEPT ![r] = p]
    /\ regionState' = [regionState EXCEPT ![r] = Live]
    /\ regionBytes' = [regionBytes EXCEPT ![r] = bytes]
    /\ UNCHANGED <<alive, endpointOwner, threadCount>>

DropRegion(r) ==
    /\ regionState[r] = Live
    /\ regionState' = [regionState EXCEPT ![r] = Reclaim]
    /\ UNCHANGED <<alive, endpointOwner, regionOwner, regionBytes, threadCount>>

ReclaimRegion(r) ==
    /\ regionState[r] = Reclaim
    /\ regionOwner' = [regionOwner EXCEPT ![r] = NoProcess]
    /\ regionState' = [regionState EXCEPT ![r] = Free]
    /\ regionBytes' = [regionBytes EXCEPT ![r] = 0]
    /\ UNCHANGED <<alive, endpointOwner, threadCount>>

AttachThread(p) ==
    /\ p \in alive
    /\ threadCount[p] < MaxThreadsPerProcess
    /\ threadCount' = [threadCount EXCEPT ![p] = @ + 1]
    /\ UNCHANGED <<alive, endpointOwner, regionOwner, regionState, regionBytes>>

DetachThread(p) ==
    /\ threadCount[p] > 0
    /\ threadCount' = [threadCount EXCEPT ![p] = @ - 1]
    /\ UNCHANGED <<alive, endpointOwner, regionOwner, regionState, regionBytes>>

ExitProcess(p) ==
    /\ p \in alive
    /\ alive' = alive \ {p}
    /\ endpointOwner' = [e \in EndpointIds |->
           IF endpointOwner[e] = p THEN NoProcess ELSE endpointOwner[e]]
    /\ regionState' = [r \in RegionIds |->
           IF regionOwner[r] = p /\ regionState[r] = Live
              THEN Reclaim ELSE regionState[r]]
    /\ threadCount' = [threadCount EXCEPT ![p] = 0]
    /\ UNCHANGED <<regionOwner, regionBytes>>

Next ==
    \/ \E p \in Processes, e \in EndpointIds: CreateEndpoint(p, e)
    \/ \E e \in EndpointIds: CloseEndpoint(e)
    \/ \E p \in Processes, r \in RegionIds, bytes \in RegionSizes:
           CreateRegion(p, r, bytes)
    \/ \E r \in RegionIds: DropRegion(r) \/ ReclaimRegion(r)
    \/ \E p \in Processes: AttachThread(p) \/ DetachThread(p) \/ ExitProcess(p)

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ \A r \in RegionIds: WF_vars(ReclaimRegion(r))

TypeOK ==
    /\ alive \in SUBSET Processes
    /\ endpointOwner \in [EndpointIds -> Processes \cup {NoProcess}]
    /\ regionOwner \in [RegionIds -> Processes \cup {NoProcess}]
    /\ regionState \in [RegionIds -> {Free, Live, Reclaim}]
    /\ regionBytes \in [RegionIds -> RegionSizes \cup {0}]
    /\ threadCount \in [Processes -> 0..MaxThreadsPerProcess]
OwnerQuotasHold ==
    \A p \in Processes:
        /\ EndpointCount(p) <= MaxEndpointsPerProcess
        /\ RegionCount(p) <= MaxRegionsPerProcess
        /\ RegionCharge(p) <= MaxRegionBytesPerProcess
        /\ threadCount[p] <= MaxThreadsPerProcess
GlobalRegionQuotaHolds == GlobalRegionCharge <= MaxGlobalRegionBytes
FreeRegionHasNoCharge ==
    \A r \in RegionIds:
        (regionState[r] = Free) <=>
            (regionOwner[r] = NoProcess /\ regionBytes[r] = 0)
ReclaimRetainsExactCharge ==
    \A r \in RegionIds:
        regionState[r] = Reclaim =>
            regionOwner[r] \in Processes /\ regionBytes[r] \in RegionSizes
ExitedProcessHasNoImmediateAuthority ==
    \A p \in Processes \ alive:
        EndpointCount(p) = 0 /\ threadCount[p] = 0
DeferredReclaimSettles ==
    \A r \in RegionIds: regionState[r] = Reclaim ~> regionState[r] = Free

=============================================================================
