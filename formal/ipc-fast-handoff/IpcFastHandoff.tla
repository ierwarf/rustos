---- MODULE IpcFastHandoff ----
EXTENDS Naturals

\* ORIGINAL production IPC producer component joint: physical sender/funder/
\* receiver loans, reader, pending wait, exact owner/FIFO join, mailbox, and
\* WakeWon vs Current commit. This is NOT MM/architecture/RSP/hardware refinement.
\* Unlike the deleted directory transaction, receiver publication and sender
\* commit are separate effects; a reverse reply may win between them.
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
          runTransfer, directedIpi, dispatches, loans, reader, pending, joined,
          originalEpoch, waitEpoch, originalGen, receiverGen, capacity, wakeWon,
          senderCommitted, remoteRecord, cold, coldFinished
vars == <<sender, receiver, blockedEndpoint, reservation, directToken, rqMember,
          runTransfer, directedIpi, dispatches, loans, reader, pending, joined,
          originalEpoch, waitEpoch, originalGen, receiverGen, capacity, wakeWon,
          senderCommitted, remoteRecord, cold, coldFinished>>
Init ==
    /\ sender = SenderRunning /\ receiver = ReceiverBlocked
    /\ blockedEndpoint = Endpoint /\ reservation = FALSE
    /\ directToken = FALSE /\ rqMember = FALSE /\ runTransfer = FALSE
    /\ directedIpi = FALSE /\ dispatches = 0
    /\ loans = FALSE /\ reader = FALSE /\ pending = FALSE /\ joined = FALSE
    /\ originalEpoch = 1 /\ waitEpoch = 1 /\ originalGen = 1 /\ receiverGen = 1
    /\ capacity \in BOOLEAN /\ wakeWon = FALSE /\ senderCommitted = FALSE
    /\ remoteRecord = FALSE /\ cold \in BOOLEAN /\ coldFinished = FALSE
ArmSender ==
    /\ ~cold /\ sender = SenderRunning /\ ~wakeWon
    /\ sender' = SenderArmed
    /\ UNCHANGED <<receiver, blockedEndpoint, reservation, directToken, rqMember,
          runTransfer, directedIpi, dispatches, loans, reader, pending, joined,
          originalEpoch, waitEpoch, originalGen, receiverGen, capacity, wakeWon,
          senderCommitted, remoteRecord>>
ReserveFrame ==
    /\ sender = SenderArmed /\ ~joined
    /\ sender' = SenderReserved /\ reservation' = TRUE
    /\ loans' = TRUE /\ reader' = TRUE /\ pending' = TRUE
    /\ UNCHANGED <<receiver, blockedEndpoint, directToken, rqMember, runTransfer,
          directedIpi, dispatches, joined, originalEpoch, waitEpoch, originalGen,
          receiverGen, capacity, wakeWon, senderCommitted, remoteRecord>>
OriginalJoin ==
    /\ sender = SenderReserved /\ receiver = ReceiverBlocked
    /\ blockedEndpoint = Endpoint /\ loans /\ reader /\ pending /\ capacity
    /\ waitEpoch = originalEpoch /\ receiverGen = originalGen /\ ~joined
ReserveExactDirect ==
    /\ OriginalJoin
    /\ receiver' = ReceiverDirect /\ directToken' = TRUE
    /\ joined' = TRUE /\ waitEpoch' = waitEpoch + 1 /\ receiverGen' = receiverGen + 1
    /\ UNCHANGED <<sender, blockedEndpoint, reservation, rqMember, runTransfer,
          directedIpi, dispatches, loans, reader, pending, originalEpoch,
          originalGen, capacity, wakeWon, senderCommitted, remoteRecord>>
ReserveExactRemote ==
    /\ OriginalJoin
    /\ receiver' = ReceiverRemote /\ runTransfer' = TRUE
    /\ joined' = TRUE /\ waitEpoch' = waitEpoch + 1 /\ receiverGen' = receiverGen + 1
    /\ UNCHANGED <<sender, blockedEndpoint, reservation, directToken, rqMember,
          directedIpi, dispatches, loans, reader, pending, originalEpoch,
          originalGen, capacity, wakeWon, senderCommitted, remoteRecord>>
PublishMailbox ==
    /\ receiver = ReceiverRemote /\ joined /\ loans /\ reader /\ ~remoteRecord
    /\ remoteRecord' = TRUE /\ directedIpi' = TRUE
    /\ UNCHANGED <<sender, receiver, blockedEndpoint, reservation, directToken,
          rqMember, runTransfer, dispatches, loans, reader, pending, joined,
          originalEpoch, waitEpoch, originalGen, receiverGen, capacity, wakeWon,
          senderCommitted>>
CloseReader ==
    /\ joined /\ reader /\ (receiver \in {ReceiverDirect, ReceiverLocal} \/ remoteRecord)
    /\ reader' = FALSE
    /\ UNCHANGED <<sender, receiver, blockedEndpoint, reservation, directToken,
          rqMember, runTransfer, directedIpi, dispatches, loans, pending, joined,
          originalEpoch, waitEpoch, originalGen, receiverGen, capacity, wakeWon,
          senderCommitted, remoteRecord>>
CommitSender ==
    /\ ~cold /\ joined /\ loans /\ pending /\ ~reader /\ ~senderCommitted
    /\ sender' = IF wakeWon THEN SenderRunning ELSE SenderBlockedReply
    /\ senderCommitted' = TRUE
    /\ UNCHANGED <<receiver, blockedEndpoint, reservation, directToken, rqMember,
          runTransfer, directedIpi, dispatches, loans, reader, pending, joined,
          originalEpoch, waitEpoch, originalGen, receiverGen, capacity, wakeWon,
          remoteRecord>>
DropOriginalLoans ==
    /\ senderCommitted /\ loans /\ ~reader
    /\ loans' = FALSE /\ pending' = FALSE
    /\ UNCHANGED <<sender, receiver, blockedEndpoint, reservation, directToken,
          rqMember, runTransfer, directedIpi, dispatches, reader, joined,
          originalEpoch, waitEpoch, originalGen, receiverGen, capacity, wakeWon,
          senderCommitted, remoteRecord>>
ReverseWake ==
    /\ ~cold /\ joined /\ ~wakeWon /\ ~senderCommitted
    /\ wakeWon' = TRUE
    /\ UNCHANGED <<sender, receiver, blockedEndpoint, reservation, directToken,
          rqMember, runTransfer, directedIpi, dispatches, loans, reader, pending,
          joined, originalEpoch, waitEpoch, originalGen, receiverGen, capacity,
          senderCommitted, remoteRecord>>
DrainRunTransfer ==
    /\ receiver = ReceiverRemote /\ reservation /\ runTransfer /\ remoteRecord
    /\ directedIpi /\ ~reader
    /\ receiver' = ReceiverLocal /\ runTransfer' = FALSE /\ rqMember' = TRUE
    /\ receiverGen' = receiverGen + 1
    /\ UNCHANGED <<sender, blockedEndpoint, reservation, directToken, directedIpi,
          dispatches, loans, reader, pending, joined, originalEpoch, waitEpoch,
          originalGen, capacity, wakeWon, senderCommitted, remoteRecord>>
DispatchDirect ==
    /\ receiver = ReceiverDirect /\ reservation /\ directToken /\ ~rqMember /\ ~reader
    /\ receiver' = ReceiverRunning /\ directToken' = FALSE
    /\ reservation' = FALSE /\ dispatches' = dispatches + 1
    /\ UNCHANGED <<sender, blockedEndpoint, rqMember, runTransfer, directedIpi,
          loans, reader, pending, joined, originalEpoch, waitEpoch, originalGen,
          receiverGen, capacity, wakeWon, senderCommitted, remoteRecord>>
DispatchRemote ==
    /\ receiver = ReceiverLocal /\ reservation /\ rqMember /\ ~reader
    /\ receiver' = ReceiverRunning /\ rqMember' = FALSE
    /\ reservation' = FALSE /\ dispatches' = dispatches + 1
    /\ UNCHANGED <<sender, blockedEndpoint, directToken, runTransfer, directedIpi,
          loans, reader, pending, joined, originalEpoch, waitEpoch, originalGen,
          receiverGen, capacity, wakeWon, senderCommitted, remoteRecord>>
CancelBeforeMutation ==
    /\ sender = SenderArmed
    /\ sender' = SenderRunning
    /\ UNCHANGED <<receiver, blockedEndpoint, reservation, directToken, rqMember,
          runTransfer, directedIpi, dispatches, loans, reader, pending, joined,
          originalEpoch, waitEpoch, originalGen, receiverGen, capacity, wakeWon,
          senderCommitted, remoteRecord>>
RollbackReservation ==
    /\ sender = SenderReserved /\ reservation /\ ~joined
    /\ receiver = ReceiverBlocked
    /\ sender' = SenderRunning /\ reservation' = FALSE
    /\ loans' = FALSE /\ reader' = FALSE /\ pending' = FALSE
    /\ UNCHANGED <<receiver, blockedEndpoint, directToken, rqMember, runTransfer,
          directedIpi, dispatches, joined, originalEpoch, waitEpoch, originalGen,
          receiverGen, capacity, wakeWon, senderCommitted, remoteRecord>>
CompletedStop ==
    /\ senderCommitted /\ ~loans /\ receiver = ReceiverRunning
    /\ UNCHANGED vars
\* Cold enqueue borrows the executing sender WITHOUT arming its later reply
\* wait. The sole typed receiver phase encloses bind, fair queue and optional
\* FIFO urgency. Later IPC runtime reply observation/wait arm is outside this
\* bounded producer model, as are physical Current/MM/RSP/terminal/reuse.
ColdCapture ==
    /\ cold /\ ~joined /\ ~loans /\ sender = SenderRunning
    /\ reservation' = TRUE /\ loans' = TRUE /\ reader' = TRUE /\ pending' = TRUE
    /\ UNCHANGED <<sender, receiver, blockedEndpoint, directToken, rqMember,
          runTransfer, directedIpi, dispatches, joined, originalEpoch, waitEpoch,
          originalGen, receiverGen, capacity, wakeWon, senderCommitted,
          remoteRecord, coldFinished>>
OriginalColdJoin ==
    /\ cold /\ sender = SenderRunning /\ receiver = ReceiverBlocked
    /\ blockedEndpoint = Endpoint /\ reservation /\ loans /\ reader /\ pending
    /\ waitEpoch = originalEpoch /\ receiverGen = originalGen /\ ~joined
ColdLocal ==
    /\ OriginalColdJoin
    /\ receiver' = ReceiverLocal /\ rqMember' = TRUE /\ joined' = TRUE
    /\ waitEpoch' = waitEpoch + 1 /\ receiverGen' = receiverGen + 1
    /\ UNCHANGED <<sender, blockedEndpoint, reservation, directToken, runTransfer,
          directedIpi, dispatches, loans, reader, pending, originalEpoch,
          originalGen, capacity, wakeWon, senderCommitted, remoteRecord, coldFinished>>
ColdRemote ==
    /\ OriginalColdJoin
    /\ receiver' = ReceiverRemote /\ runTransfer' = TRUE /\ joined' = TRUE
    /\ waitEpoch' = waitEpoch + 1 /\ receiverGen' = receiverGen + 1
    /\ UNCHANGED <<sender, blockedEndpoint, reservation, directToken, rqMember,
          directedIpi, dispatches, loans, reader, pending, originalEpoch,
          originalGen, capacity, wakeWon, senderCommitted, remoteRecord, coldFinished>>
ColdFinish ==
    /\ cold /\ joined /\ loans /\ pending /\ ~reader /\ ~coldFinished
    /\ coldFinished' = TRUE /\ loans' = FALSE /\ pending' = FALSE
    /\ UNCHANGED <<sender, receiver, blockedEndpoint, reservation, directToken,
          rqMember, runTransfer, directedIpi, dispatches, reader, joined,
          originalEpoch, waitEpoch, originalGen, receiverGen, capacity, wakeWon,
          senderCommitted, remoteRecord>>
ColdCompletedStop == cold /\ coldFinished /\ receiver = ReceiverRunning /\ UNCHANGED vars
Next ==
    /\ cold' = cold
    /\ (((CompletedStop \/ ArmSender \/ ReserveFrame \/ ReserveExactDirect \/ ReserveExactRemote
        \/ PublishMailbox \/ CloseReader \/ CommitSender \/ DropOriginalLoans
        \/ ReverseWake \/ DrainRunTransfer \/ DispatchDirect \/ DispatchRemote
        \/ CancelBeforeMutation \/ RollbackReservation) /\ UNCHANGED coldFinished)
        \/ ColdCapture \/ ColdLocal \/ ColdRemote \/ ColdFinish \/ ColdCompletedStop)
Spec == Init /\ [][Next]_vars
TypeOK ==
    /\ sender \in {SenderRunning, SenderArmed, SenderReserved, SenderBlockedReply}
    /\ receiver \in {ReceiverBlocked, ReceiverDirect, ReceiverRemote, ReceiverLocal, ReceiverRunning}
    /\ blockedEndpoint = Endpoint
    /\ reservation \in BOOLEAN /\ directToken \in BOOLEAN /\ rqMember \in BOOLEAN
    /\ runTransfer \in BOOLEAN /\ directedIpi \in BOOLEAN /\ dispatches \in Nat
    /\ loans \in BOOLEAN /\ reader \in BOOLEAN /\ pending \in BOOLEAN /\ joined \in BOOLEAN
    /\ capacity \in BOOLEAN /\ wakeWon \in BOOLEAN /\ senderCommitted \in BOOLEAN
    /\ remoteRecord \in BOOLEAN /\ cold \in BOOLEAN /\ coldFinished \in BOOLEAN
    /\ originalEpoch \in Nat /\ waitEpoch \in Nat /\ originalGen \in Nat /\ receiverGen \in Nat
DirectHasExactReservation == receiver = ReceiverDirect => reservation
DirectNeverHasRunqueueCustody == receiver = ReceiverDirect => ~rqMember
TokenNamesOnlyDirectCustody == directToken => receiver = ReceiverDirect
RunTransferNamesOnlyRemoteCustody == runTransfer => receiver = ReceiverRemote
RemoteTransferHasDirectedIpi == receiver = ReceiverRemote /\ ~reader => directedIpi
TransferCustodyIsExclusive == ~(directToken /\ runTransfer)
RemoteNeverHasRunqueueCustody == receiver = ReceiverRemote => ~rqMember
LocalOwnsNoTransfer == receiver = ReceiverLocal => rqMember /\ ~runTransfer /\ ~directToken
DispatchIsAtMostOnce == dispatches <= 1
AtomicCallTransfer == joined => sender \in {SenderReserved, SenderBlockedReply, SenderRunning}
ReservedFrameHasNoReceiverMutation == sender = SenderReserved /\ ~joined => receiver = ReceiverBlocked
RunningSenderOwnsNoReservation == ~cold /\ sender = SenderRunning /\ ~wakeWon => ~reservation
OriginalLoansUntilFinalEffect == joined /\ ~senderCommitted /\ ~coldFinished => loans /\ pending
RefusedJoinHasNoEffects == ~joined => waitEpoch = originalEpoch /\ receiverGen = originalGen
OriginalTypedEpochConsumedOnce == joined => waitEpoch = originalEpoch + 1
OriginalJoinRequiresBoundedCustody == joined /\ ~cold => capacity
WakeWonNeverOverwritten == senderCommitted /\ wakeWon => sender = SenderRunning
ReaderExcludesExecution == reader => receiver # ReceiverRunning /\ ~senderCommitted
MailboxPrecedesRemoteExecution == receiver \in {ReceiverLocal, ReceiverRunning} /\ remoteRecord => directedIpi
ColdNeverCommitsSender == cold => sender = SenderRunning /\ ~senderCommitted
ColdFundingBeforeWake == cold /\ receiver # ReceiverBlocked /\ dispatches = 0 => reservation
ColdFinishAfterOriginalEffect == coldFinished => joined /\ ~reader /\ ~loans /\ ~pending
====
