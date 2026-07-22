---------------------- MODULE MsiVectorLifecycle ----------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Owner: kernel HAL x86 MSI allocator and leaf handler table.
Linearization points: monotonic vector allocation and one-shot handler CAS.
Only an allocated vector with an installed handler may yield a device route;
vectors are permanent and never rebound or recycled in this lifecycle.
***************************************************************************)

CONSTANTS FirstVector, LastVector, Devices
Vectors == FirstVector..LastVector
NoDevice == "none"
VARIABLES nextVector, allocated, handlerOwner, apicReady, routed, routeOwner
vars == <<nextVector, allocated, handlerOwner, apicReady, routed, routeOwner>>

Init ==
    /\ nextVector = FirstVector /\ allocated = {} /\ apicReady = FALSE
    /\ routed = {}
    /\ handlerOwner = [v \in Vectors |-> NoDevice]
    /\ routeOwner = [v \in Vectors |-> NoDevice]

Allocate ==
    /\ nextVector <= LastVector
    /\ allocated' = allocated \cup {nextVector}
    /\ nextVector' = nextVector + 1
    /\ UNCHANGED <<handlerOwner, apicReady, routed, routeOwner>>

InitializeApic ==
    /\ ~apicReady /\ apicReady' = TRUE
    /\ UNCHANGED <<nextVector, allocated, handlerOwner, routed, routeOwner>>

Register(v, d) ==
    /\ v \in allocated /\ handlerOwner[v] = NoDevice /\ d \in Devices
    /\ handlerOwner' = [handlerOwner EXCEPT ![v] = d]
    /\ UNCHANGED <<nextVector, allocated, apicReady, routed, routeOwner>>

Route(v, d) ==
    /\ apicReady /\ v \in allocated /\ handlerOwner[v] = d /\ v \notin routed
    /\ routed' = routed \cup {v}
    /\ routeOwner' = [routeOwner EXCEPT ![v] = d]
    /\ UNCHANGED <<nextVector, allocated, handlerOwner, apicReady>>

Next ==
    \/ Allocate \/ InitializeApic
    \/ \E v \in Vectors, d \in Devices: Register(v, d) \/ Route(v, d)
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ nextVector \in FirstVector..(LastVector + 1)
    /\ allocated \in SUBSET Vectors /\ apicReady \in BOOLEAN
    /\ routed \in SUBSET Vectors
    /\ handlerOwner \in [Vectors -> Devices \cup {NoDevice}]
    /\ routeOwner \in [Vectors -> Devices \cup {NoDevice}]
HandlerRequiresAllocation == \A v \in Vectors: handlerOwner[v] # NoDevice => v \in allocated
RouteRequiresExactHandler == \A v \in routed: routeOwner[v] = handlerOwner[v] /\ handlerOwner[v] # NoDevice
NoUnallocatedRoute == routed \subseteq allocated
AllocatedPrefixMatchesCursor == allocated = FirstVector..(nextVector - 1)
MessageAuthorityRequiresReadyApic == routed # {} => apicReady

=============================================================================
