--------------------------- MODULE IvshmemPairing ---------------------------
EXTENDS Naturals

(*******************************************************************************
Launch-private ivshmem doorbell pairing.

The QEMU ivshmem server stream allocates peer IDs in connection order; it
contains no guest identity field.  Therefore the host launcher must not start
the GUI DVM until the broker has observed the RustOS compositor connection.
Peer 0 is RustOS, peer 1 is the one paired GUI DVM, and either disconnect tears
down the complete topology.  No reconnect/reassignment transition exists.

Concrete owner:
  libs/driver-domain-host/src/ivshmem.rs
    IvshmemDoorbellServer::wait_for_peer_count
  tools/xtask/src/kvm.rs
    spawn_guests / DVM_DISPLAY_FIRST_PEER_TIMEOUT
*******************************************************************************)

CONSTANTS MaxPeers

VARIABLES hostPeer,
          dvmLaunched,
          dvmPeer,
          topologyFailed

vars == <<hostPeer, dvmLaunched, dvmPeer, topologyFailed>>

Init ==
    /\ hostPeer = FALSE
    /\ dvmLaunched = FALSE
    /\ dvmPeer = FALSE
    /\ topologyFailed = FALSE

(*******************************************************************************
The first accept is explicitly awaited by the launcher.  No DVM process is
eligible to start until peer 0 has been established as RustOS.
*******************************************************************************)
ConnectRustOS ==
    /\ ~hostPeer
    /\ ~dvmLaunched
    /\ ~topologyFailed
    /\ hostPeer' = TRUE
    /\ UNCHANGED <<dvmLaunched, dvmPeer, topologyFailed>>

LaunchGuiDvm ==
    /\ hostPeer
    /\ ~dvmLaunched
    /\ ~topologyFailed
    /\ dvmLaunched' = TRUE
    /\ UNCHANGED <<hostPeer, dvmPeer, topologyFailed>>

ConnectGuiDvm ==
    /\ hostPeer
    /\ dvmLaunched
    /\ ~dvmPeer
    /\ ~topologyFailed
    /\ dvmPeer' = TRUE
    /\ UNCHANGED <<hostPeer, dvmLaunched, topologyFailed>>

(*******************************************************************************
A departed peer has invalidated its eventfd and capability assignment.  The
broker fails closed: it releases both identities and never reopens admission
on that socket.
*******************************************************************************)
FailClosed ==
    /\ (hostPeer \/ dvmPeer)
    /\ ~topologyFailed
    /\ hostPeer' = FALSE
    /\ dvmPeer' = FALSE
    /\ topologyFailed' = TRUE
    /\ UNCHANGED dvmLaunched

Idle == UNCHANGED vars

Next ==
    \/ ConnectRustOS
    \/ LaunchGuiDvm
    \/ ConnectGuiDvm
    \/ FailClosed
    \/ Idle

Spec == Init /\ [][Next]_vars /\ WF_vars(ConnectRustOS) /\
        WF_vars(LaunchGuiDvm) /\ WF_vars(ConnectGuiDvm)

TypeOK ==
    /\ MaxPeers = 2
    /\ hostPeer \in BOOLEAN
    /\ dvmLaunched \in BOOLEAN
    /\ dvmPeer \in BOOLEAN
    /\ topologyFailed \in BOOLEAN

GuiDvmNeverClaimsPeerZero == dvmLaunched => hostPeer \/ topologyFailed

ConnectedDvmHasRustOSPeerZero == dvmPeer => /\ hostPeer /\ dvmLaunched

FailedTopologyHasNoLivePeer == topologyFailed => /\ ~hostPeer /\ ~dvmPeer

NoReconnectOrReassignment == topologyFailed ~> []topologyFailed

LaunchedGuiDvmSettles ==
    [](dvmLaunched /\ ~topologyFailed => <>(dvmPeer \/ topologyFailed))
=============================================================================
