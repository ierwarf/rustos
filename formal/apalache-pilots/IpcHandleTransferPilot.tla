----------------------- MODULE IpcHandleTransferPilot -----------------------

(***************************************************************************
Typed symbolic refinement pilot for one descriptor from
formal/ipc-handle-transfer/IpcHandleTransfer.tla. The full TLC model retains
batch all-or-nothing and peer/caller teardown interleavings.
***************************************************************************)

VARIABLES
    \* @type: Str;
    transferState,
    \* @type: Bool;
    registryPresent,
    \* @type: Str;
    messageState

vars == <<transferState, registryPresent, messageState>>

Init ==
    /\ transferState = "source"
    /\ registryPresent = FALSE
    /\ messageState = "none"

Export ==
    /\ transferState = "source"
    /\ transferState' = "exported"
    /\ registryPresent' = TRUE
    /\ UNCHANGED messageState

Enqueue ==
    /\ transferState = "exported"
    /\ registryPresent
    /\ transferState' = "queued"
    /\ messageState' = "queued"
    /\ UNCHANGED registryPresent

Receive ==
    /\ transferState = "queued"
    /\ registryPresent
    /\ transferState' = "received"
    /\ messageState' = "received"
    /\ UNCHANGED registryPresent

Install ==
    /\ transferState = "received"
    /\ registryPresent
    /\ transferState' = "installed"
    /\ registryPresent' = FALSE
    /\ messageState' = "none"

Drop ==
    /\ transferState \in {"exported", "queued", "received"}
    /\ transferState' = "dropped"
    /\ registryPresent' = FALSE
    /\ messageState' = "cancelled"

Next == Export \/ Enqueue \/ Receive \/ Install \/ Drop

TypeOK ==
    /\ transferState \in {"source", "exported", "queued", "received", "installed", "dropped"}
    /\ registryPresent \in BOOLEAN
    /\ messageState \in {"none", "queued", "received", "cancelled"}

RegistryExactlyTracksTransfer ==
    registryPresent <=> transferState \in {"exported", "queued", "received"}
TerminalCannotPinAuthority ==
    transferState \in {"installed", "dropped"} => ~registryPresent
=============================================================================
