------------------------- MODULE IpcEndpointOwnership -------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models the user-visible IPC endpoint authority boundary.

Concrete owners:
  * kernel/ipc-runtime/src/ipc/mod.rs
  * kernel/compat/src/user/syscall/linux/ipc_ops.rs
  * kernel/ps/src/user/handles/table.rs

An endpoint created by a user process may be served by a worker in that same
process. A queued reply capability is bound to the owning process at enqueue
time, before the raw reply id becomes observable. A foreign-process task may
probe raw numeric endpoint or reply values, but it must not dequeue a message,
install transferred handles, complete the reply, or change the endpoint state.
When the owner process exits, queued or received authority is terminally
revoked. Descriptor duplication also rejects sparse targets above the process
descriptor ceiling without growing the table.
*******************************************************************************)

CONSTANTS Tasks, MaxFd

OwnerMain == 1
OwnerWorker == 2
Foreign == 3
Caller == 4
NoTask == 0

OwnerProcess == 1
ForeignProcess == 2

ProcessOf(task) ==
    IF task \in {OwnerMain, OwnerWorker} THEN OwnerProcess ELSE ForeignProcess

NoMessage == "none"
Queued == "queued"
Received == "received"
Replied == "replied"
Cancelled == "cancelled"

NoReply == "none"
LiveReply == "live"
UsedReply == "used"

NoTransfer == "none"
QueuedTransfer == "queued"
ReceivedTransfer == "received"
InstalledTransfer == "installed"
DroppedTransfer == "dropped"

NoAttempt == "none"
ForeignReceive == "foreign-receive"
ForeignReply == "foreign-reply"
ForeignHandleReceive == "foreign-handle-receive"
HugeDup == "huge-dup"

VARIABLES endpointOwnerProcess,
          messageState,
          replyState,
          replyReceiverProcess,
          deliveredTo,
          transferState,
          fdTable,
          requestedFd,
          lastAttempt

vars == <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess, deliveredTo,
          transferState, fdTable, requestedFd, lastAttempt>>

Init ==
    /\ endpointOwnerProcess = OwnerProcess
    /\ messageState = NoMessage
    /\ replyState = NoReply
    /\ replyReceiverProcess = NoTask
    /\ deliveredTo = NoTask
    /\ transferState = NoTransfer
    /\ fdTable = {3}
    /\ requestedFd = 3
    /\ lastAttempt = NoAttempt

EnqueueWithTransfer ==
    /\ messageState = NoMessage
    /\ replyState = NoReply
    /\ messageState' = Queued
    /\ replyState' = LiveReply
    /\ replyReceiverProcess' = endpointOwnerProcess
    /\ transferState' = QueuedTransfer
    /\ lastAttempt' = NoAttempt
    /\ UNCHANGED <<endpointOwnerProcess, deliveredTo, fdTable, requestedFd>>

OwnerProcessReceives ==
    /\ messageState = Queued
    /\ messageState' = Received
    /\ deliveredTo' \in {OwnerMain, OwnerWorker}
    /\ transferState' = ReceivedTransfer
    /\ lastAttempt' = NoAttempt
    /\ UNCHANGED <<endpointOwnerProcess, replyState, replyReceiverProcess, fdTable, requestedFd>>

ForeignReceiveAttempt ==
    /\ messageState = Queued
    /\ lastAttempt' = ForeignReceive
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, fdTable, requestedFd>>

ForeignHandleReceiveAttempt ==
    /\ messageState = Queued
    /\ transferState = QueuedTransfer
    /\ lastAttempt' = ForeignHandleReceive
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, fdTable, requestedFd>>

ForeignReplyAttempt ==
    /\ replyState = LiveReply
    /\ lastAttempt' = ForeignReply
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, fdTable, requestedFd>>

OwnerProcessReplies ==
    /\ messageState = Received
    /\ replyState = LiveReply
    /\ replyReceiverProcess = endpointOwnerProcess
    /\ ProcessOf(deliveredTo) = endpointOwnerProcess
    \* `recvmsg` installs the transferred batch before the reply capability
    \* is exposed to the service.  Otherwise a replied terminal message could
    \* retain an unowned ReceivedTransfer indefinitely.
    /\ transferState = InstalledTransfer
    /\ messageState' = Replied
    /\ replyState' = UsedReply
    /\ lastAttempt' = NoAttempt
    /\ UNCHANGED <<endpointOwnerProcess, replyReceiverProcess, deliveredTo, transferState,
                   fdTable, requestedFd>>

OwnerProcessInstallsTransfer ==
    /\ messageState = Received
    /\ ProcessOf(deliveredTo) = endpointOwnerProcess
    /\ transferState = ReceivedTransfer
    /\ transferState' = InstalledTransfer
    /\ lastAttempt' = NoAttempt
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, fdTable, requestedFd>>

CancelQueued ==
    /\ messageState = Queued
    /\ messageState' = Cancelled
    /\ replyState' = UsedReply
    /\ transferState' = DroppedTransfer
    /\ lastAttempt' = NoAttempt
    /\ UNCHANGED <<endpointOwnerProcess, replyReceiverProcess, deliveredTo, fdTable,
                   requestedFd>>

OwnerProcessExits ==
    /\ messageState \in {Queued, Received}
    /\ messageState' = Cancelled
    /\ replyState' = UsedReply
    /\ transferState' = DroppedTransfer
    /\ lastAttempt' = NoAttempt
    /\ UNCHANGED <<endpointOwnerProcess, replyReceiverProcess, deliveredTo, fdTable,
                   requestedFd>>

RejectSparseDup ==
    /\ requestedFd = MaxFd
    /\ requestedFd' = MaxFd + 1
    /\ lastAttempt' = HugeDup
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, fdTable>>

BoundedDup ==
    /\ requestedFd \in 3..MaxFd
    /\ requestedFd' \in 3..MaxFd
    /\ fdTable' = fdTable \cup {requestedFd'}
    /\ lastAttempt' = NoAttempt
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState>>

Next ==
    \/ EnqueueWithTransfer
    \/ OwnerProcessReceives
    \/ ForeignReceiveAttempt
    \/ ForeignHandleReceiveAttempt
    \/ ForeignReplyAttempt
    \/ OwnerProcessReplies
    \/ OwnerProcessInstallsTransfer
    \/ CancelQueued
    \/ OwnerProcessExits
    \/ RejectSparseDup
    \/ BoundedDup

TypeOK ==
    /\ Tasks = {OwnerMain, OwnerWorker, Foreign, Caller}
    /\ endpointOwnerProcess \in {OwnerProcess, ForeignProcess}
    /\ messageState \in {NoMessage, Queued, Received, Replied, Cancelled}
    /\ replyState \in {NoReply, LiveReply, UsedReply}
    /\ replyReceiverProcess \in {OwnerProcess, ForeignProcess, NoTask}
    /\ deliveredTo \in Tasks \cup {NoTask}
    /\ transferState \in {NoTransfer, QueuedTransfer, ReceivedTransfer,
                            InstalledTransfer, DroppedTransfer}
    /\ fdTable \subseteq 3..MaxFd
    /\ requestedFd \in 3..(MaxFd + 1)
    /\ lastAttempt \in {NoAttempt, ForeignReceive, ForeignReply,
                          ForeignHandleReceive, HugeDup}

QueuedReplyIsBoundToEndpointOwnerProcess ==
    replyState = LiveReply => replyReceiverProcess = endpointOwnerProcess

OnlyEndpointOwnerProcessReceives ==
    messageState = Received => ProcessOf(deliveredTo) = endpointOwnerProcess

ForeignProbesAreNonDestructive ==
    /\ lastAttempt = ForeignReceive =>
        /\ messageState = Queued
        /\ deliveredTo = NoTask
        /\ transferState = QueuedTransfer
    /\ lastAttempt = ForeignHandleReceive =>
        /\ messageState = Queued
        /\ deliveredTo = NoTask
        /\ transferState = QueuedTransfer
    /\ lastAttempt = ForeignReply =>
        /\ replyState = LiveReply
        /\ messageState \in {Queued, Received}
        /\ replyReceiverProcess = endpointOwnerProcess

OnlyOwnerProcessCanInstallTransferredHandle ==
    transferState = InstalledTransfer => ProcessOf(deliveredTo) = endpointOwnerProcess

ReplyCannotCompleteBeforeOwnerProcessReceive ==
    replyState = UsedReply /\ messageState = Replied =>
        /\ ProcessOf(deliveredTo) = endpointOwnerProcess
        /\ replyReceiverProcess = endpointOwnerProcess
        /\ transferState = InstalledTransfer

SparseFdRequestDoesNotGrowTable ==
    requestedFd = MaxFd + 1 => fdTable \subseteq 3..MaxFd

TerminalTransferHasNoQueuedAuthority ==
    transferState \in {InstalledTransfer, DroppedTransfer} =>
        messageState # Queued

TerminalMessageHasNoDetachedTransfer ==
    messageState \in {Replied, Cancelled} =>
        transferState \in {InstalledTransfer, DroppedTransfer}

=============================================================================
