---------------------- MODULE MsiVectorLifecycle ----------------------
EXTENDS Naturals, FiniteSets

(***************************************************************************
Owner: kernel HAL x86 MSI allocator and leaf handler table.
Linearization points: free-slot lease CAS, exact handler CAS, revocable commit,
device route, final transport publication, and permanent retention or atomic
failure rollback. A committed vector is not permanent until the last fallible
provider/transport publication succeeds.
***************************************************************************)

CONSTANTS FirstVector, LastVector, Devices
Vectors == FirstVector..LastVector
NoDevice == "none"
VARIABLES leased, committed, permanent, handlerOwner, apicReady, routed,
          routeOwner, publishedDevices, failedDevices
vars == <<leased, committed, permanent, handlerOwner, apicReady, routed,
          routeOwner, publishedDevices, failedDevices>>

Init ==
    /\ leased = {} /\ committed = {} /\ permanent = {} /\ apicReady = FALSE
    /\ routed = {}
    /\ publishedDevices = {} /\ failedDevices = {}
    /\ handlerOwner = [v \in Vectors |-> NoDevice]
    /\ routeOwner = [v \in Vectors |-> NoDevice]

Allocate(v) ==
    /\ v \notin leased
    /\ leased' = leased \cup {v}
    /\ UNCHANGED <<committed, permanent, handlerOwner, apicReady, routed,
                    routeOwner, publishedDevices, failedDevices>>

InitializeApic ==
    /\ ~apicReady /\ apicReady' = TRUE
    /\ UNCHANGED <<leased, committed, permanent, handlerOwner, routed,
                    routeOwner, publishedDevices, failedDevices>>

Register(v, d) ==
    /\ v \in leased /\ v \notin committed
    /\ handlerOwner[v] = NoDevice /\ d \in Devices /\ d \notin failedDevices
    /\ handlerOwner' = [handlerOwner EXCEPT ![v] = d]
    /\ UNCHANGED <<leased, committed, permanent, apicReady, routed, routeOwner,
                    publishedDevices, failedDevices>>

Commit(v, d) ==
    /\ v \in leased /\ v \notin committed /\ handlerOwner[v] = d
    /\ committed' = committed \cup {v}
    /\ UNCHANGED <<leased, permanent, handlerOwner, apicReady, routed, routeOwner,
                    publishedDevices, failedDevices>>

Rollback(v, d) ==
    /\ v \in leased /\ v \notin committed /\ v \notin routed
    /\ handlerOwner[v] \in {NoDevice, d}
    /\ leased' = leased \ {v}
    /\ handlerOwner' = [handlerOwner EXCEPT ![v] = NoDevice]
    /\ UNCHANGED <<committed, permanent, apicReady, routed, routeOwner,
                    publishedDevices, failedDevices>>

Route(v, d) ==
    /\ apicReady /\ v \in committed /\ handlerOwner[v] = d /\ v \notin routed
    /\ routed' = routed \cup {v}
    /\ routeOwner' = [routeOwner EXCEPT ![v] = d]
    /\ UNCHANGED <<leased, committed, permanent, handlerOwner, apicReady,
                    publishedDevices, failedDevices>>

PublishDevice(d) ==
    /\ d \in Devices /\ d \notin failedDevices /\ d \notin publishedDevices
    /\ \E v \in routed: routeOwner[v] = d
    /\ publishedDevices' = publishedDevices \cup {d}
    /\ UNCHANGED <<leased, committed, permanent, handlerOwner, apicReady,
                    routed, routeOwner, failedDevices>>

RetainPermanent(v, d) ==
    /\ v \in committed /\ routeOwner[v] = d /\ d \in publishedDevices
    /\ permanent' = permanent \cup {v}
    /\ UNCHANGED <<leased, committed, handlerOwner, apicReady, routed,
                    routeOwner, publishedDevices, failedDevices>>

FailPublication(d) ==
    LET Owned == {v \in Vectors: handlerOwner[v] = d /\ v \notin permanent} IN
    /\ d \in Devices /\ d \notin publishedDevices /\ d \notin failedDevices
    /\ failedDevices' = failedDevices \cup {d}
    /\ leased' = leased \ Owned
    /\ committed' = committed \ Owned
    /\ routed' = routed \ Owned
    /\ handlerOwner' = [v \in Vectors |-> IF v \in Owned THEN NoDevice ELSE handlerOwner[v]]
    /\ routeOwner' = [v \in Vectors |-> IF v \in Owned THEN NoDevice ELSE routeOwner[v]]
    /\ UNCHANGED <<permanent, apicReady, publishedDevices>>

Next ==
    \/ InitializeApic
    \/ \E v \in Vectors: Allocate(v)
    \/ \E v \in Vectors, d \in Devices:
        Register(v, d) \/ Commit(v, d) \/ Rollback(v, d) \/ Route(v, d) \/
        RetainPermanent(v, d)
    \/ \E d \in Devices: PublishDevice(d) \/ FailPublication(d)
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ leased \in SUBSET Vectors /\ committed \in SUBSET Vectors
    /\ permanent \in SUBSET Vectors
    /\ apicReady \in BOOLEAN
    /\ routed \in SUBSET Vectors
    /\ handlerOwner \in [Vectors -> Devices \cup {NoDevice}]
    /\ routeOwner \in [Vectors -> Devices \cup {NoDevice}]
    /\ publishedDevices \in SUBSET Devices /\ failedDevices \in SUBSET Devices
HandlerRequiresLease == \A v \in Vectors: handlerOwner[v] # NoDevice => v \in leased
CommittedRequiresExactHandler ==
    \A v \in committed: handlerOwner[v] # NoDevice
RouteRequiresExactHandler == \A v \in routed: routeOwner[v] = handlerOwner[v] /\ handlerOwner[v] # NoDevice
CommittedStaysLeased == committed \subseteq leased
PermanentRequiresPublishedRoute ==
    \A v \in permanent: v \in routed /\ routeOwner[v] \in publishedDevices
NoUncommittedRoute == routed \subseteq committed
FreeSlotHasNoHandler ==
    \A v \in Vectors \ leased: handlerOwner[v] = NoDevice
MessageAuthorityRequiresReadyApic == routed # {} => apicReady
FailedPublicationHasNoAuthority ==
    \A d \in failedDevices:
        /\ d \notin publishedDevices
        /\ \A v \in Vectors: handlerOwner[v] # d /\ routeOwner[v] # d

=============================================================================
