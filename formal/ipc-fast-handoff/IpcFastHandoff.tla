---- MODULE IpcFastHandoff ----
EXTENDS Naturals

\* Safety refinement for the Phase-5 same- and cross-CPU call transfer. The endpoint
\* identity is decided while the receiver is still BlockedOnRecv. Reservation
\* moves the sender and receiver together only after bounded ordering
\* admission; a rejection restores the frame reservation before either task
\* changes lifecycle custody.

CONSTANT Endpoint

SenderRunning == "sender-running"
SenderArmed == "sender-armed"
SenderReserved == "sender-reserved"
SenderBlockedReply == "sender-blocked-reply"
ReceiverBlocked == "receiver-blocked-recv"
ReceiverDirect == "receiver-direct"
ReceiverRemote == "receiver-remote-queued"
ReceiverLocal == "receiver-local"
ReceiverRunning == "receiver-running"

VARIABLES sender, receiver, blockedEndpoint, reservation, directToken, rqMember,
          runTransfer, directedIpi, dispatches

vars == <<sender, receiver, blockedEndpoint, reservation, directToken, rqMember,
          runTransfer, directedIpi, dispatches>>

Init ==
    /\ sender = SenderRunning
    /\ receiver = ReceiverBlocked
    /\ blockedEndpoint = Endpoint
    /\ reservation = FALSE
    /\ directToken = FALSE
    /\ rqMember = FALSE
    /\ runTransfer = FALSE
    /\ directedIpi = FALSE
    /\ dispatches = 0

ArmSender ==
    /\ sender = SenderRunning
    /\ sender' = SenderArmed
    /\ UNCHANGED <<receiver, blockedEndpoint, reservation, directToken, rqMember,
                    runTransfer, directedIpi, dispatches>>

ReserveFrame ==
    /\ sender = SenderArmed
    /\ sender' = SenderReserved
    /\ reservation' = TRUE
    /\ UNCHANGED <<receiver, blockedEndpoint, directToken, rqMember, runTransfer,
                    directedIpi, dispatches>>

ReserveExactDirect ==
    /\ sender = SenderReserved
    /\ receiver = ReceiverBlocked
    /\ blockedEndpoint = Endpoint
    /\ sender' = SenderBlockedReply
    /\ receiver' = ReceiverDirect
    /\ directToken' = TRUE
    /\ UNCHANGED <<blockedEndpoint, reservation, rqMember, runTransfer,
                    directedIpi, dispatches>>

ReserveExactRemote ==
    /\ sender = SenderReserved
    /\ receiver = ReceiverBlocked
    /\ blockedEndpoint = Endpoint
    /\ sender' = SenderBlockedReply
    /\ receiver' = ReceiverRemote
    /\ runTransfer' = TRUE
    /\ directedIpi' = TRUE
    /\ UNCHANGED <<blockedEndpoint, reservation, directToken, rqMember, dispatches>>

DrainRunTransfer ==
    /\ receiver = ReceiverRemote
    /\ reservation
    /\ runTransfer
    /\ directedIpi
    /\ receiver' = ReceiverLocal
    /\ runTransfer' = FALSE
    /\ rqMember' = TRUE
    /\ UNCHANGED <<sender, blockedEndpoint, reservation, directToken, directedIpi,
                    dispatches>>

DispatchDirect ==
    /\ receiver = ReceiverDirect
    /\ reservation
    /\ directToken
    /\ ~rqMember
    /\ receiver' = ReceiverRunning
    /\ directToken' = FALSE
    /\ reservation' = FALSE
    /\ dispatches' = dispatches + 1
    /\ UNCHANGED <<sender, blockedEndpoint, rqMember, runTransfer, directedIpi>>

DispatchRemote ==
    /\ receiver = ReceiverLocal
    /\ reservation
    /\ rqMember
    /\ receiver' = ReceiverRunning
    /\ rqMember' = FALSE
    /\ reservation' = FALSE
    /\ dispatches' = dispatches + 1
    /\ UNCHANGED <<sender, blockedEndpoint, directToken, runTransfer, directedIpi>>

CancelBeforeMutation ==
    /\ sender = SenderArmed
    /\ sender' = SenderRunning
    /\ UNCHANGED <<receiver, blockedEndpoint, reservation, directToken, rqMember,
                    runTransfer, directedIpi, dispatches>>

RollbackReservation ==
    /\ sender = SenderReserved
    /\ reservation
    /\ receiver = ReceiverBlocked
    /\ sender' = SenderRunning
    /\ reservation' = FALSE
    /\ UNCHANGED <<receiver, blockedEndpoint, directToken, rqMember, runTransfer,
                    directedIpi, dispatches>>

TerminalStutter ==
    /\ receiver = ReceiverRunning
    /\ UNCHANGED vars

Next == ArmSender \/ ReserveFrame \/ ReserveExactDirect \/ ReserveExactRemote
        \/ DrainRunTransfer \/ DispatchDirect \/ DispatchRemote
        \/ CancelBeforeMutation \/ RollbackReservation \/ TerminalStutter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ sender \in {SenderRunning, SenderArmed, SenderReserved, SenderBlockedReply}
    /\ receiver \in {ReceiverBlocked, ReceiverDirect, ReceiverRemote, ReceiverLocal,
                     ReceiverRunning}
    /\ blockedEndpoint = Endpoint
    /\ reservation \in BOOLEAN
    /\ directToken \in BOOLEAN
    /\ rqMember \in BOOLEAN
    /\ runTransfer \in BOOLEAN
    /\ directedIpi \in BOOLEAN
    /\ dispatches \in Nat

DirectHasExactReservation == receiver = ReceiverDirect => reservation
DirectNeverHasRunqueueCustody == receiver = ReceiverDirect => ~rqMember
TokenNamesOnlyDirectCustody == directToken => receiver = ReceiverDirect
RunTransferNamesOnlyRemoteCustody == runTransfer => receiver = ReceiverRemote
RemoteTransferHasDirectedIpi == receiver = ReceiverRemote => directedIpi
TransferCustodyIsExclusive == ~(directToken /\ runTransfer)
RemoteNeverHasRunqueueCustody == receiver = ReceiverRemote => ~rqMember
LocalOwnsNoTransfer == receiver = ReceiverLocal => rqMember /\ ~runTransfer /\ ~directToken
DispatchIsAtMostOnce == dispatches <= 1
AtomicCallTransfer == receiver \in {ReceiverDirect, ReceiverRemote, ReceiverLocal,
                                    ReceiverRunning}
                      => sender = SenderBlockedReply
ReservedFrameHasNoReceiverMutation == sender = SenderReserved => receiver = ReceiverBlocked
RunningSenderOwnsNoReservation == sender = SenderRunning => ~reservation
====
