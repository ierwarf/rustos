------------------------- MODULE RuntimeControlAuthority -------------------------
EXTENDS Naturals

(*******************************************************************************
Models server-side authorization for runtimed's Unix control socket.

Concrete owners:
  services/runtimed/src/socket.rs
  services/runtimed/src/spawn.rs

The request wire is not an identity claim. The server obtains an immutable peer
PID from SO_PEERCRED, then revalidates either the current uiserver service owner
or a live logical-admin launch record derived from the signed registry.
UI-readiness remains uiserver-only. Restart, service revoke, and process exit
withdraw the corresponding authorization before dispatch.
*******************************************************************************)

Ops == {"snapshot", "launch", "terminate", "ui-ready"}
Roles == {"foreign", "uiserver", "logical-admin"}
Outcomes == {"pending", "admitted", "denied"}

VARIABLES op,
          role,
          peerPidValid,
          uiserverOwnerLive,
          logicalAdminLive,
          outcome

vars == <<op, role, peerPidValid, uiserverOwnerLive, logicalAdminLive, outcome>>

Authorized ==
    peerPidValid /\
        IF op = "ui-ready"
        THEN role = "uiserver" /\ uiserverOwnerLive
        ELSE
            \/ role = "uiserver" /\ uiserverOwnerLive
            \/ role = "logical-admin" /\ logicalAdminLive

Init ==
    /\ op \in Ops
    /\ role \in Roles
    /\ peerPidValid \in BOOLEAN
    /\ uiserverOwnerLive \in BOOLEAN
    /\ logicalAdminLive \in BOOLEAN
    /\ outcome = "pending"

RevokeUiserver ==
    /\ outcome = "pending"
    /\ uiserverOwnerLive
    /\ uiserverOwnerLive' = FALSE
    /\ UNCHANGED <<op, role, peerPidValid, logicalAdminLive, outcome>>

ExitLogicalAdmin ==
    /\ outcome = "pending"
    /\ logicalAdminLive
    /\ logicalAdminLive' = FALSE
    /\ UNCHANGED <<op, role, peerPidValid, uiserverOwnerLive, outcome>>

Dispatch ==
    /\ outcome = "pending"
    /\ outcome' = IF Authorized THEN "admitted" ELSE "denied"
    /\ UNCHANGED <<op, role, peerPidValid, uiserverOwnerLive, logicalAdminLive>>

Next == RevokeUiserver \/ ExitLogicalAdmin \/ Dispatch

TypeOK ==
    /\ op \in Ops
    /\ role \in Roles
    /\ peerPidValid \in BOOLEAN
    /\ uiserverOwnerLive \in BOOLEAN
    /\ logicalAdminLive \in BOOLEAN
    /\ outcome \in Outcomes

AdmissionRequiresKernelStampedPeer ==
    outcome = "admitted" => peerPidValid

UiReadyRequiresLiveUiserverOwner ==
    outcome = "admitted" /\ op = "ui-ready" =>
        /\ role = "uiserver"
        /\ uiserverOwnerLive

MutationRequiresLivePrivilegedRole ==
    outcome = "admitted" /\ op \in {"launch", "terminate"} =>
        \/ role = "uiserver" /\ uiserverOwnerLive
        \/ role = "logical-admin" /\ logicalAdminLive

ForeignPeerNeverDispatches ==
    role = "foreign" => outcome # "admitted"

RevokedRoleNeverDispatches ==
    outcome = "admitted" =>
        /\ role = "uiserver" => uiserverOwnerLive
        /\ role = "logical-admin" => logicalAdminLive

=============================================================================
