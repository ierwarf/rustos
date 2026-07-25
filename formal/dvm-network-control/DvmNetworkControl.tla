------------------------------ MODULE DvmNetworkControl -------------------------
EXTENDS Naturals

(*******************************************************************************
Models authenticated control-lease gating for the fixed DVM Ethernet ring.

Concrete owners and source anchors:
  * L0-authenticated RDI1 session lifecycle:
    libs/driver-domain-host/src/lib.rs
  * input protocol decoding and lifecycle handoff:
    services/inputd/src/dvm_protocol.rs
    services/inputd/src/main.rs
  * exact service-owned epoch admission and revocation:
    services/netd/src/main.rs
  * capability-gated ivshmem transport lease:
    kernel/io-manager/src/io/dvm_network.rs

The Ethernet ivshmem header/counters are intentionally not an authority
channel: after mapping, the DVM may write its data-plane counters. L0 emits
RDI1 SESSION_START only after the launch-bound HMAC control handshake and
SESSION_END while disconnect cleanup is still serialized by the same epoch.
Inputd validates that protocol and hands an exact epoch transition to netd over
a bounded authenticated IPC call. Netd owns lifecycle policy; ring0 only
enforces the capability-gated transport lease selected by netd. The model
permits arbitrary DVM writes before, during, and after a lease, but only a live
authenticated lease permits RustOS to accept a network transmit or receive. An
old cleanup must not revoke a replacement lease.

This model deliberately abstracts fixed-ring cursor bounds; those are checked
by dvm-network-ring/DvmNetworkRing. It instead proves the lifecycle condition
that the ring model intentionally did not include.
*******************************************************************************)

CONSTANTS Epochs, MaxPackets, MaxDenied, MaxDvmWrites

NoEpoch == 0
Idle == "idle"
Mapped == "mapped"
Activated == "activated"
Revoked == "revoked"
StaleRevoked == "stale-revoked"
TxAccepted == "tx-accepted"
RxAccepted == "rx-accepted"
TxDenied == "tx-denied"
RxDenied == "rx-denied"
DvmWrite == "dvm-write"

AcceptedOutcomes == {TxAccepted, RxAccepted}
DeniedOutcomes == {TxDenied, RxDenied}

VARIABLES mapped,
          activeEpoch,
          acceptedTx,
          acceptedRx,
          deniedTx,
          deniedRx,
          dvmWrites,
          lastOutcome,
          lastAccessEpoch,
          staleRevokedEpoch,
          leaseBeforeAction,
          issuedEpochs

vars == <<mapped, activeEpoch, acceptedTx, acceptedRx, deniedTx, deniedRx,
          dvmWrites, lastOutcome, lastAccessEpoch, staleRevokedEpoch,
          leaseBeforeAction, issuedEpochs>>

TransportAvailable == mapped /\ activeEpoch \in Epochs

Init ==
    /\ mapped = FALSE
    /\ activeEpoch = NoEpoch
    /\ acceptedTx = 0
    /\ acceptedRx = 0
    /\ deniedTx = 0
    /\ deniedRx = 0
    /\ dvmWrites = 0
    /\ lastOutcome = Idle
    /\ lastAccessEpoch = NoEpoch
    /\ staleRevokedEpoch = NoEpoch
    /\ leaseBeforeAction = NoEpoch
    /\ issuedEpochs = {}

\* The source allows a session marker before a late PCI mapping. Mapping alone
\* still does not establish availability; it only makes a pre-existing L0
\* lease usable.
Install ==
    /\ ~mapped
    /\ mapped' = TRUE
    /\ lastOutcome' = Mapped
    /\ leaseBeforeAction' = activeEpoch
    /\ UNCHANGED <<activeEpoch, acceptedTx, acceptedRx, deniedTx, deniedRx,
                  dvmWrites, lastAccessEpoch, staleRevokedEpoch, issuedEpochs>>

\* Only an L0-authenticated SESSION_START may take this transition. DVM data
\* writes do not have an action that changes activeEpoch.
Activate(epoch) ==
    /\ epoch \in Epochs
    /\ epoch \notin issuedEpochs
    /\ activeEpoch' = epoch
    /\ issuedEpochs' = issuedEpochs \cup {epoch}
    /\ lastOutcome' = Activated
    /\ leaseBeforeAction' = activeEpoch
    /\ UNCHANGED <<mapped, acceptedTx, acceptedRx, deniedTx, deniedRx,
                  dvmWrites, lastAccessEpoch, staleRevokedEpoch>>

RevokeExact(epoch) ==
    /\ epoch \in Epochs
    /\ activeEpoch = epoch
    /\ activeEpoch' = NoEpoch
    /\ lastOutcome' = Revoked
    /\ leaseBeforeAction' = activeEpoch
    /\ UNCHANGED <<mapped, acceptedTx, acceptedRx, deniedTx, deniedRx,
                  dvmWrites, lastAccessEpoch, staleRevokedEpoch, issuedEpochs>>

\* A delayed end marker either names a retired lease or arrives after the
\* active lease was already cleared. It cannot alter the current lease.
IgnoreStaleRevoke(epoch) ==
    /\ epoch \in Epochs
    /\ activeEpoch # epoch
    /\ lastOutcome' = StaleRevoked
    /\ staleRevokedEpoch' = epoch
    /\ leaseBeforeAction' = activeEpoch
    /\ UNCHANGED <<mapped, activeEpoch, acceptedTx, acceptedRx, deniedTx,
                  deniedRx, dvmWrites, lastAccessEpoch, issuedEpochs>>

KernelTransmit ==
    /\ TransportAvailable
    /\ acceptedTx < MaxPackets
    /\ acceptedTx' = acceptedTx + 1
    /\ lastOutcome' = TxAccepted
    /\ lastAccessEpoch' = activeEpoch
    /\ leaseBeforeAction' = activeEpoch
    /\ UNCHANGED <<mapped, activeEpoch, acceptedRx, deniedTx, deniedRx,
                  dvmWrites, staleRevokedEpoch, issuedEpochs>>

KernelReceive ==
    /\ TransportAvailable
    /\ acceptedRx < MaxPackets
    /\ acceptedRx' = acceptedRx + 1
    /\ lastOutcome' = RxAccepted
    /\ lastAccessEpoch' = activeEpoch
    /\ leaseBeforeAction' = activeEpoch
    /\ UNCHANGED <<mapped, activeEpoch, acceptedTx, deniedTx, deniedRx,
                  dvmWrites, staleRevokedEpoch, issuedEpochs>>

\* An unavailable mapped aperture returns NoDevice rather than behaving as a
\* busy but live ring. These actions model attempted RustOS packet access.
DenyTransmit ==
    /\ mapped
    /\ activeEpoch = NoEpoch
    /\ deniedTx < MaxDenied
    /\ deniedTx' = deniedTx + 1
    /\ lastOutcome' = TxDenied
    /\ leaseBeforeAction' = activeEpoch
    /\ UNCHANGED <<mapped, activeEpoch, acceptedTx, acceptedRx, deniedRx,
                  dvmWrites, lastAccessEpoch, staleRevokedEpoch, issuedEpochs>>

DenyReceive ==
    /\ mapped
    /\ activeEpoch = NoEpoch
    /\ deniedRx < MaxDenied
    /\ deniedRx' = deniedRx + 1
    /\ lastOutcome' = RxDenied
    /\ leaseBeforeAction' = activeEpoch
    /\ UNCHANGED <<mapped, activeEpoch, acceptedTx, acceptedRx, deniedTx,
                  dvmWrites, lastAccessEpoch, staleRevokedEpoch, issuedEpochs>>

\* The DVM may mutate its fixed data plane at any time, including after
\* revocation. This never grants, clears, or replaces a RustOS control lease.
UntrustedDvmWrite ==
    /\ mapped
    /\ dvmWrites < MaxDvmWrites
    /\ dvmWrites' = dvmWrites + 1
    /\ lastOutcome' = DvmWrite
    /\ leaseBeforeAction' = activeEpoch
    /\ UNCHANGED <<mapped, activeEpoch, acceptedTx, acceptedRx, deniedTx,
                  deniedRx, lastAccessEpoch, staleRevokedEpoch, issuedEpochs>>

Next ==
    \/ Install
    \/ \E epoch \in Epochs: Activate(epoch)
    \/ \E epoch \in Epochs: RevokeExact(epoch)
    \/ \E epoch \in Epochs: IgnoreStaleRevoke(epoch)
    \/ KernelTransmit
    \/ KernelReceive
    \/ DenyTransmit
    \/ DenyReceive
    \/ UntrustedDvmWrite

TypeOK ==
    /\ mapped \in BOOLEAN
    /\ activeEpoch \in Epochs \cup {NoEpoch}
    /\ issuedEpochs \subseteq Epochs
    /\ acceptedTx \in 0..MaxPackets
    /\ acceptedRx \in 0..MaxPackets
    /\ deniedTx \in 0..MaxDenied
    /\ deniedRx \in 0..MaxDenied
    /\ dvmWrites \in 0..MaxDvmWrites
    /\ lastOutcome \in {Idle, Mapped, Activated, Revoked, StaleRevoked,
                         TxAccepted, RxAccepted, TxDenied, RxDenied, DvmWrite}
    /\ lastAccessEpoch \in Epochs \cup {NoEpoch}
    /\ staleRevokedEpoch \in Epochs \cup {NoEpoch}
    /\ leaseBeforeAction \in Epochs \cup {NoEpoch}

AcceptedTrafficHasALiveAuthenticatedLease ==
    lastOutcome \in AcceptedOutcomes =>
        /\ TransportAvailable
        /\ lastAccessEpoch = activeEpoch

MappingAloneNeverAcceptsTraffic ==
    mapped /\ activeEpoch = NoEpoch => lastOutcome \notin AcceptedOutcomes

ExactRevocationClearsNetworkAuthority ==
    lastOutcome = Revoked =>
        /\ leaseBeforeAction \in Epochs
        /\ activeEpoch = NoEpoch

StaleRevocationPreservesCurrentLease ==
    lastOutcome = StaleRevoked =>
        /\ activeEpoch = leaseBeforeAction
        /\ activeEpoch # staleRevokedEpoch

UnavailablePacketAttemptsFailClosed ==
    lastOutcome \in DeniedOutcomes =>
        /\ mapped
        /\ activeEpoch = NoEpoch

DvmWritesCannotChangeControlAuthority ==
    lastOutcome = DvmWrite => activeEpoch = leaseBeforeAction

ActiveControlEpochWasIssuedExactlyOnce ==
    activeEpoch \in Epochs => activeEpoch \in issuedEpochs

Spec == Init /\ [][Next]_vars
=============================================================================
