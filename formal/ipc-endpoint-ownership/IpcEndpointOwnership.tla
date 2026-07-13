------------------------- MODULE IpcEndpointOwnership -------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models the user-visible IPC endpoint authority boundary.

Concrete owners:
  * kernel/ipc-runtime/src/ipc/mod.rs
  * kernel/compat/src/user/syscall/linux/ipc_ops.rs
  * kernel/ps/src/user/handles/table.rs

An endpoint created by a user task has exactly one receiving task.  A queued
reply capability is bound to that same task at enqueue time, before the raw
reply id becomes observable.  A foreign task may probe raw numeric endpoint or
reply values, but it must not dequeue a message, install transferred handles,
complete the reply, or change the endpoint state.  Descriptor duplication also
rejects sparse targets above the process descriptor ceiling without growing the
table.
*******************************************************************************)

CONSTANTS Tasks, MaxFd

Owner == 1
Foreign == 2
Caller == 3
NoTask == 0

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

VARIABLES endpointOwner,
          messageState,
          replyState,
          replyReceiver,
          deliveredTo,
          transferState,
          fdTable,
          requestedFd,
          lastAttempt

vars == <<endpointOwner, messageState, replyState, replyReceiver, deliveredTo,
          transferState, fdTable, requestedFd, lastAttempt>>

Init ==
    /\ endpointOwner = Owner
    /\ messageState = NoMessage
    /\ replyState = NoReply
    /\ replyReceiver = NoTask
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
    /\ replyReceiver' = endpointOwner
    /\ transferState' = QueuedTransfer
    /\ lastAttempt' = NoAttempt
    /\ UNCHANGED <<endpointOwner, deliveredTo, fdTable, requestedFd>>

OwnerReceives ==
    /\ messageState = Queued
    /\ messageState' = Received
    /\ deliveredTo' = endpointOwner
    /\ transferState' = ReceivedTransfer
    /\ lastAttempt' = NoAttempt
    /\ UNCHANGED <<endpointOwner, replyState, replyReceiver, fdTable, requestedFd>>

ForeignReceiveAttempt ==
    /\ messageState = Queued
    /\ lastAttempt' = ForeignReceive
    /\ UNCHANGED <<endpointOwner, messageState, replyState, replyReceiver,
                   deliveredTo, transferState, fdTable, requestedFd>>

ForeignHandleReceiveAttempt ==
    /\ messageState = Queued
    /\ transferState = QueuedTransfer
    /\ lastAttempt' = ForeignHandleReceive
    /\ UNCHANGED <<endpointOwner, messageState, replyState, replyReceiver,
                   deliveredTo, transferState, fdTable, requestedFd>>

ForeignReplyAttempt ==
    /\ replyState = LiveReply
    /\ lastAttempt' = ForeignReply
    /\ UNCHANGED <<endpointOwner, messageState, replyState, replyReceiver,
                   deliveredTo, transferState, fdTable, requestedFd>>

OwnerReplies ==
    /\ messageState = Received
    /\ replyState = LiveReply
    /\ replyReceiver = endpointOwner
    /\ deliveredTo = endpointOwner
    /\ messageState' = Replied
    /\ replyState' = UsedReply
    /\ lastAttempt' = NoAttempt
    /\ UNCHANGED <<endpointOwner, replyReceiver, deliveredTo, transferState,
                   fdTable, requestedFd>>

OwnerInstallsTransfer ==
    /\ messageState = Received
    /\ deliveredTo = endpointOwner
    /\ transferState = ReceivedTransfer
    /\ transferState' = InstalledTransfer
    /\ lastAttempt' = NoAttempt
    /\ UNCHANGED <<endpointOwner, messageState, replyState, replyReceiver,
                   deliveredTo, fdTable, requestedFd>>

CancelQueued ==
    /\ messageState = Queued
    /\ messageState' = Cancelled
    /\ replyState' = UsedReply
    /\ transferState' = DroppedTransfer
    /\ lastAttempt' = NoAttempt
    /\ UNCHANGED <<endpointOwner, replyReceiver, deliveredTo, fdTable,
                   requestedFd>>

RejectSparseDup ==
    /\ requestedFd = MaxFd
    /\ requestedFd' = MaxFd + 1
    /\ lastAttempt' = HugeDup
    /\ UNCHANGED <<endpointOwner, messageState, replyState, replyReceiver,
                   deliveredTo, transferState, fdTable>>

BoundedDup ==
    /\ requestedFd \in 3..MaxFd
    /\ requestedFd' \in 3..MaxFd
    /\ fdTable' = fdTable \cup {requestedFd'}
    /\ lastAttempt' = NoAttempt
    /\ UNCHANGED <<endpointOwner, messageState, replyState, replyReceiver,
                   deliveredTo, transferState>>

Next ==
    \/ EnqueueWithTransfer
    \/ OwnerReceives
    \/ ForeignReceiveAttempt
    \/ ForeignHandleReceiveAttempt
    \/ ForeignReplyAttempt
    \/ OwnerReplies
    \/ OwnerInstallsTransfer
    \/ CancelQueued
    \/ RejectSparseDup
    \/ BoundedDup

TypeOK ==
    /\ Tasks = {Owner, Foreign, Caller}
    /\ endpointOwner \in Tasks
    /\ messageState \in {NoMessage, Queued, Received, Replied, Cancelled}
    /\ replyState \in {NoReply, LiveReply, UsedReply}
    /\ replyReceiver \in Tasks \cup {NoTask}
    /\ deliveredTo \in Tasks \cup {NoTask}
    /\ transferState \in {NoTransfer, QueuedTransfer, ReceivedTransfer,
                            InstalledTransfer, DroppedTransfer}
    /\ fdTable \subseteq 3..MaxFd
    /\ requestedFd \in 3..(MaxFd + 1)
    /\ lastAttempt \in {NoAttempt, ForeignReceive, ForeignReply,
                          ForeignHandleReceive, HugeDup}

QueuedReplyIsBoundToEndpointOwner ==
    replyState = LiveReply => replyReceiver = endpointOwner

OnlyEndpointOwnerReceives ==
    messageState = Received => deliveredTo = endpointOwner

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
        /\ replyReceiver = endpointOwner

OnlyOwnerCanInstallTransferredHandle ==
    transferState = InstalledTransfer => deliveredTo = endpointOwner

ReplyCannotCompleteBeforeOwnedReceive ==
    replyState = UsedReply /\ messageState = Replied =>
        /\ deliveredTo = endpointOwner
        /\ replyReceiver = endpointOwner

SparseFdRequestDoesNotGrowTable ==
    requestedFd = MaxFd + 1 => fdTable \subseteq 3..MaxFd

TerminalTransferHasNoQueuedAuthority ==
    transferState \in {InstalledTransfer, DroppedTransfer} =>
        messageState # Queued

=============================================================================
