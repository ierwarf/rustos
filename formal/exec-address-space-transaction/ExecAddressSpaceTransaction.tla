------------------- MODULE ExecAddressSpaceTransaction -------------------
EXTENDS Naturals

(***************************************************************************
Exec spans process-table reservation, scheduler admission/CR3 installation,
and process-state ownership transfer.  Exit may win before installation or
arrive after authorization, but an installed root always retains an exact
owner and an authorized installation must complete its transfer.
***************************************************************************)

Idle == "idle"
Reserved == "reserved"
Quiesced == "quiesced"
Authorized == "authorized"
RootInstalled == "root-installed"
Owned == "owned"
Cancelled == "cancelled"

NoOwner == "none"
PreparedOwner == "prepared-owner"
ProcessOwner == "process-owner"

VARIABLES phase, exiting, retiredMarker, reservationValid, activeRoot,
          rootOwner, ownershipCommitted, installedOverRetirement

vars == <<phase, exiting, retiredMarker, reservationValid, activeRoot,
          rootOwner, ownershipCommitted, installedOverRetirement>>

Init ==
    /\ phase = Idle /\ exiting = FALSE /\ retiredMarker = FALSE
    /\ reservationValid = FALSE /\ activeRoot = FALSE
    /\ rootOwner = NoOwner /\ ownershipCommitted = FALSE
    /\ installedOverRetirement = FALSE

BeginExec ==
    /\ phase = Idle /\ ~exiting /\ ~retiredMarker
    /\ phase' = Reserved /\ reservationValid' = TRUE
    /\ rootOwner' = PreparedOwner
    /\ UNCHANGED <<exiting, retiredMarker, activeRoot, ownershipCommitted,
                    installedOverRetirement>>

Quiesce ==
    /\ phase = Reserved
    /\ phase' = Quiesced
    /\ UNCHANGED <<exiting, retiredMarker, reservationValid, activeRoot,
                    rootOwner, ownershipCommitted, installedOverRetirement>>

PublishExit ==
    /\ phase \in {Reserved, Quiesced, Authorized, RootInstalled}
    /\ exiting' = TRUE /\ retiredMarker' = TRUE
    /\ UNCHANGED <<phase, reservationValid, activeRoot, rootOwner,
                    ownershipCommitted, installedOverRetirement>>

Authorize ==
    /\ phase = Quiesced /\ reservationValid
    /\ ~exiting /\ ~retiredMarker
    /\ phase' = Authorized
    /\ UNCHANGED <<exiting, retiredMarker, reservationValid, activeRoot,
                    rootOwner, ownershipCommitted, installedOverRetirement>>

InstallRoot ==
    /\ phase = Authorized /\ reservationValid
    /\ ~retiredMarker
    /\ phase' = RootInstalled /\ activeRoot' = TRUE
    /\ installedOverRetirement' = retiredMarker
    /\ UNCHANGED <<exiting, retiredMarker, reservationValid, rootOwner,
                    ownershipCommitted>>

TransferOwnership ==
    /\ phase = RootInstalled /\ reservationValid /\ activeRoot
    /\ phase' = Owned /\ rootOwner' = ProcessOwner
    /\ ownershipCommitted' = TRUE /\ reservationValid' = FALSE
    /\ UNCHANGED <<exiting, retiredMarker, activeRoot,
                    installedOverRetirement>>

CancelBeforeInstall ==
    /\ phase \in {Reserved, Quiesced, Authorized}
    /\ exiting \/ retiredMarker
    /\ phase' = Cancelled /\ reservationValid' = FALSE
    /\ rootOwner' = NoOwner
    /\ UNCHANGED <<exiting, retiredMarker, activeRoot, ownershipCommitted,
                    installedOverRetirement>>

Terminal ==
    /\ phase \in {Owned, Cancelled}
    /\ UNCHANGED vars

Next == BeginExec \/ Quiesce \/ PublishExit \/ Authorize \/ InstallRoot
        \/ TransferOwnership \/ CancelBeforeInstall \/ Terminal

Spec == Init /\ [][Next]_vars /\ WF_vars(TransferOwnership)

TypeOK ==
    /\ phase \in {Idle, Reserved, Quiesced, Authorized, RootInstalled,
                   Owned, Cancelled}
    /\ exiting \in BOOLEAN /\ retiredMarker \in BOOLEAN
    /\ reservationValid \in BOOLEAN /\ activeRoot \in BOOLEAN
    /\ rootOwner \in {NoOwner, PreparedOwner, ProcessOwner}
    /\ ownershipCommitted \in BOOLEAN
    /\ installedOverRetirement \in BOOLEAN

ActiveRootAlwaysOwned == activeRoot => rootOwner # NoOwner
RootInstallNeverOverwritesRetirement == ~installedOverRetirement
OwnedPhaseHasProcessOwner ==
    phase = Owned => ownershipCommitted /\ rootOwner = ProcessOwner
InstalledRootKeepsReservationUntilTransfer ==
    phase = RootInstalled => reservationValid /\ rootOwner = PreparedOwner

InstalledRootEventuallyTransfers ==
    phase = RootInstalled ~> phase = Owned

=============================================================================
