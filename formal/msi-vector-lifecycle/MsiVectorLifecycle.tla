---------------------- MODULE MsiVectorLifecycle ----------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Owner: kernel HAL x86 MSI allocator and leaf handler table.
Linearization points: free-slot lease CAS, exact handler CAS, lease commit, and
exact unpublished rollback. Failed MSI-X setup clears its handler before the
slot becomes reusable. Only a committed lease with its exact handler may yield
a device route; committed vectors remain permanent in this kernel lifetime.
***************************************************************************)

CONSTANTS FirstVector, LastVector, Devices
Vectors == FirstVector..LastVector
NoDevice == "none"
VARIABLES leased, committed, handlerOwner, apicReady, routed, routeOwner
vars == <<leased, committed, handlerOwner, apicReady, routed, routeOwner>>

Init ==
    /\ leased = {} /\ committed = {} /\ apicReady = FALSE
    /\ routed = {}
    /\ handlerOwner = [v \in Vectors |-> NoDevice]
    /\ routeOwner = [v \in Vectors |-> NoDevice]

Allocate(v) ==
    /\ v \notin leased
    /\ leased' = leased \cup {v}
    /\ UNCHANGED <<committed, handlerOwner, apicReady, routed, routeOwner>>

InitializeApic ==
    /\ ~apicReady /\ apicReady' = TRUE
    /\ UNCHANGED <<leased, committed, handlerOwner, routed, routeOwner>>

Register(v, d) ==
    /\ v \in leased /\ v \notin committed
    /\ handlerOwner[v] = NoDevice /\ d \in Devices
    /\ handlerOwner' = [handlerOwner EXCEPT ![v] = d]
    /\ UNCHANGED <<leased, committed, apicReady, routed, routeOwner>>

Commit(v, d) ==
    /\ v \in leased /\ v \notin committed /\ handlerOwner[v] = d
    /\ committed' = committed \cup {v}
    /\ UNCHANGED <<leased, handlerOwner, apicReady, routed, routeOwner>>

Rollback(v, d) ==
    /\ v \in leased /\ v \notin committed /\ v \notin routed
    /\ handlerOwner[v] \in {NoDevice, d}
    /\ leased' = leased \ {v}
    /\ handlerOwner' = [handlerOwner EXCEPT ![v] = NoDevice]
    /\ UNCHANGED <<committed, apicReady, routed, routeOwner>>

Route(v, d) ==
    /\ apicReady /\ v \in committed /\ handlerOwner[v] = d /\ v \notin routed
    /\ routed' = routed \cup {v}
    /\ routeOwner' = [routeOwner EXCEPT ![v] = d]
    /\ UNCHANGED <<leased, committed, handlerOwner, apicReady>>

Next ==
    \/ InitializeApic
    \/ \E v \in Vectors: Allocate(v)
    \/ \E v \in Vectors, d \in Devices:
        Register(v, d) \/ Commit(v, d) \/ Rollback(v, d) \/ Route(v, d)
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ leased \in SUBSET Vectors /\ committed \in SUBSET Vectors
    /\ apicReady \in BOOLEAN
    /\ routed \in SUBSET Vectors
    /\ handlerOwner \in [Vectors -> Devices \cup {NoDevice}]
    /\ routeOwner \in [Vectors -> Devices \cup {NoDevice}]
HandlerRequiresLease == \A v \in Vectors: handlerOwner[v] # NoDevice => v \in leased
CommittedRequiresExactHandler ==
    \A v \in committed: handlerOwner[v] # NoDevice
RouteRequiresExactHandler == \A v \in routed: routeOwner[v] = handlerOwner[v] /\ handlerOwner[v] # NoDevice
CommittedStaysLeased == committed \subseteq leased
NoUncommittedRoute == routed \subseteq committed
FreeSlotHasNoHandler ==
    \A v \in Vectors \ leased: handlerOwner[v] = NoDevice
MessageAuthorityRequiresReadyApic == routed # {} => apicReady

=============================================================================
