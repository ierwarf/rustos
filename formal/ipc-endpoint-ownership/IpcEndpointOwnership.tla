------------------------- MODULE IpcEndpointOwnership -------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models the user-visible IPC endpoint authority boundary.

Concrete owners:
  * kernel/ipc-runtime/src/ipc/mod.rs
  * kernel/compat/src/user/syscall/linux/ipc_ops.rs
  * kernel/compat/src/user/syscall/linux/net_broker_ops.rs
  * kernel/compat/src/user/syscall/linux/service_ops/vfs_socket.rs
  * kernel/ps/src/user/handles/table.rs
  * services/netd/src/main.rs

An endpoint created by a user process may be served by a worker in that same
process. A queued reply capability is bound to the owning process at enqueue
time, before the raw reply id becomes observable. A foreign-process task may
probe raw numeric endpoint or reply values, but it must not dequeue a message,
install transferred handles, complete the reply, or change the endpoint state.
When the owner process exits, the endpoint itself dies and queued, received,
or already installed process-local transfer authority is terminally revoked.
No later enqueue can revive the dead numeric endpoint. Descriptor duplication
also rejects sparse targets above the process
descriptor ceiling without growing the table.
An externally opened vfsd object or one/two freshly allocated netd tokens are
either installed in that bounded table or closed on malformed metadata, local
descriptor-admission failure, and pair copyout failure.
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
FullInstall == "full-install"
RemoteFullInstall == "remote-full-install"
MalformedRemote == "malformed-remote"
RemotePairFullInstall == "remote-pair-full-install"

NoErrno == "none"
PermissionErrno == "eperm"
BadDescriptorErrno == "ebadf"
CapacityErrno == "emfile"
ProtocolErrno == "einval"

NoRemote == "none"
OpenRemote == "open"
InstalledRemote == "installed"
ClosedRemote == "closed"
OpenRemotePair == "open-pair"
InstalledRemotePair == "installed-pair"
ClosedRemotePair == "closed-pair"

VARIABLES endpointOwnerProcess,
          messageState,
          replyState,
          replyReceiverProcess,
          deliveredTo,
          transferState,
          fdTable,
          fdSnapshot,
          requestedFd,
          lastAttempt,
          lastErrno,
          remoteState,
          remoteFdCount

vars == <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess, deliveredTo,
          transferState, fdTable, fdSnapshot, requestedFd, lastAttempt, lastErrno,
          remoteState, remoteFdCount>>

Init ==
    /\ endpointOwnerProcess = OwnerProcess
    /\ messageState = NoMessage
    /\ replyState = NoReply
    /\ replyReceiverProcess = NoTask
    /\ deliveredTo = NoTask
    /\ transferState = NoTransfer
    /\ fdTable = {3}
    /\ fdSnapshot = {3}
    /\ requestedFd = 3
    /\ lastAttempt = NoAttempt
    /\ lastErrno = NoErrno
    /\ remoteState = NoRemote
    /\ remoteFdCount = 0

EnqueueWithTransfer ==
    /\ endpointOwnerProcess # NoTask
    /\ messageState = NoMessage
    /\ replyState = NoReply
    /\ messageState' = Queued
    /\ replyState' = LiveReply
    /\ replyReceiverProcess' = endpointOwnerProcess
    /\ transferState' = QueuedTransfer
    /\ lastAttempt' = NoAttempt
    /\ lastErrno' = NoErrno
    /\ UNCHANGED <<endpointOwnerProcess, deliveredTo, fdTable, fdSnapshot, requestedFd,
                   remoteState, remoteFdCount>>

OwnerProcessReceives ==
    /\ messageState = Queued
    /\ messageState' = Received
    /\ deliveredTo' \in {OwnerMain, OwnerWorker}
    /\ transferState' = ReceivedTransfer
    /\ lastAttempt' = NoAttempt
    /\ lastErrno' = NoErrno
    /\ UNCHANGED <<endpointOwnerProcess, replyState, replyReceiverProcess, fdTable, fdSnapshot,
                   requestedFd, remoteState, remoteFdCount>>

ForeignReceiveAttempt ==
    /\ messageState = Queued
    /\ lastAttempt' = ForeignReceive
    /\ lastErrno' = PermissionErrno
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, fdTable, fdSnapshot, requestedFd,
                   remoteState, remoteFdCount>>

ForeignHandleReceiveAttempt ==
    /\ messageState = Queued
    /\ transferState = QueuedTransfer
    /\ lastAttempt' = ForeignHandleReceive
    /\ lastErrno' = PermissionErrno
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, fdTable, fdSnapshot, requestedFd,
                   remoteState, remoteFdCount>>

ForeignReplyAttempt ==
    /\ replyState = LiveReply
    /\ lastAttempt' = ForeignReply
    /\ lastErrno' = PermissionErrno
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, fdTable, fdSnapshot, requestedFd,
                   remoteState, remoteFdCount>>

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
    /\ lastErrno' = NoErrno
    /\ UNCHANGED <<endpointOwnerProcess, replyReceiverProcess, deliveredTo, transferState,
                   fdTable, fdSnapshot, requestedFd, remoteState, remoteFdCount>>

OwnerProcessInstallsTransfer ==
    /\ messageState = Received
    /\ ProcessOf(deliveredTo) = endpointOwnerProcess
    /\ transferState = ReceivedTransfer
    /\ transferState' = InstalledTransfer
    /\ lastAttempt' = NoAttempt
    /\ lastErrno' = NoErrno
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, fdTable, fdSnapshot, requestedFd, remoteState,
                   remoteFdCount>>

CancelQueued ==
    /\ messageState = Queued
    /\ messageState' = Cancelled
    /\ replyState' = UsedReply
    /\ transferState' = DroppedTransfer
    /\ lastAttempt' = NoAttempt
    /\ lastErrno' = NoErrno
    /\ UNCHANGED <<endpointOwnerProcess, replyReceiverProcess, deliveredTo, fdTable, fdSnapshot,
                   requestedFd, remoteState, remoteFdCount>>

OwnerProcessExits ==
    /\ endpointOwnerProcess # NoTask
    /\ endpointOwnerProcess' = NoTask
    /\ messageState' = IF messageState \in {Queued, Received} THEN Cancelled ELSE messageState
    /\ replyState' = IF replyState = LiveReply THEN UsedReply ELSE replyState
    /\ transferState' = IF transferState = NoTransfer THEN NoTransfer ELSE DroppedTransfer
    /\ lastAttempt' = NoAttempt
    /\ lastErrno' = NoErrno
    /\ UNCHANGED <<replyReceiverProcess, deliveredTo, fdTable, fdSnapshot,
                   requestedFd, remoteState, remoteFdCount>>

RejectSparseDup ==
    /\ requestedFd = MaxFd
    /\ requestedFd' = MaxFd + 1
    /\ fdSnapshot' = fdTable
    /\ lastAttempt' = HugeDup
    /\ lastErrno' = BadDescriptorErrno
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, fdTable, remoteState, remoteFdCount>>

BoundedDup ==
    /\ requestedFd \in 3..MaxFd
    /\ requestedFd' \in 3..MaxFd
    /\ fdTable' = fdTable \cup {requestedFd'}
    /\ fdSnapshot' = fdTable'
    /\ lastAttempt' = NoAttempt
    /\ lastErrno' = NoErrno
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, remoteState, remoteFdCount>>

InstallAtFreeFd ==
    /\ fdTable # 3..MaxFd
    /\ requestedFd' \in (3..MaxFd) \ fdTable
    /\ fdTable' = fdTable \cup {requestedFd'}
    /\ fdSnapshot' = fdTable'
    /\ lastAttempt' = NoAttempt
    /\ lastErrno' = NoErrno
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, remoteState, remoteFdCount>>

RejectFullInstall ==
    /\ fdTable = 3..MaxFd
    /\ fdSnapshot' = fdTable
    /\ lastAttempt' = FullInstall
    /\ lastErrno' = CapacityErrno
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, fdTable, requestedFd, remoteState,
                   remoteFdCount>>

OpenRemoteObject ==
    /\ remoteState = NoRemote
    /\ remoteState' = OpenRemote
    /\ remoteFdCount' = 0
    /\ lastAttempt' = NoAttempt
    /\ lastErrno' = NoErrno
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, fdTable, fdSnapshot, requestedFd>>

InstallRemoteObject ==
    /\ remoteState = OpenRemote
    /\ fdTable # 3..MaxFd
    /\ requestedFd' \in (3..MaxFd) \ fdTable
    /\ fdTable' = fdTable \cup {requestedFd'}
    /\ fdSnapshot' = fdTable'
    /\ remoteState' = InstalledRemote
    /\ remoteFdCount' = 1
    /\ lastAttempt' = NoAttempt
    /\ lastErrno' = NoErrno
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState>>

RejectRemoteAtFullTable ==
    /\ remoteState = OpenRemote
    /\ fdTable = 3..MaxFd
    /\ remoteState' = ClosedRemote
    /\ remoteFdCount' = 0
    /\ fdSnapshot' = fdTable
    /\ lastAttempt' = RemoteFullInstall
    /\ lastErrno' = CapacityErrno
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, fdTable, requestedFd>>

OpenRemoteObjectPair ==
    /\ remoteState = NoRemote
    /\ remoteState' = OpenRemotePair
    /\ remoteFdCount' = 0
    /\ lastAttempt' = NoAttempt
    /\ lastErrno' = NoErrno
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, fdTable, fdSnapshot, requestedFd>>

InstallRemoteObjectPair ==
    /\ remoteState = OpenRemotePair
    /\ Cardinality((3..MaxFd) \ fdTable) >= 2
    /\ \E leftFd, rightFd \in (3..MaxFd) \ fdTable:
        /\ leftFd # rightFd
        /\ requestedFd' = leftFd
        /\ fdTable' = fdTable \cup {leftFd, rightFd}
        /\ fdSnapshot' = fdTable'
        /\ remoteState' = InstalledRemotePair
        /\ remoteFdCount' = 2
        /\ lastAttempt' = NoAttempt
        /\ lastErrno' = NoErrno
        /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState,
                       replyReceiverProcess, deliveredTo, transferState>>

RejectRemotePairWithoutCapacity ==
    /\ remoteState = OpenRemotePair
    /\ Cardinality((3..MaxFd) \ fdTable) < 2
    /\ remoteState' = ClosedRemotePair
    /\ remoteFdCount' = 0
    /\ fdSnapshot' = fdTable
    /\ lastAttempt' = RemotePairFullInstall
    /\ lastErrno' = CapacityErrno
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, fdTable, requestedFd>>

RejectMalformedRemote ==
    /\ remoteState \in {OpenRemote, OpenRemotePair}
    /\ remoteState' = IF remoteState = OpenRemote THEN ClosedRemote ELSE ClosedRemotePair
    /\ remoteFdCount' = 0
    /\ fdSnapshot' = fdTable
    /\ lastAttempt' = MalformedRemote
    /\ lastErrno' = ProtocolErrno
    /\ UNCHANGED <<endpointOwnerProcess, messageState, replyState, replyReceiverProcess,
                   deliveredTo, transferState, fdTable, requestedFd>>

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
    \/ InstallAtFreeFd
    \/ RejectFullInstall
    \/ OpenRemoteObject
    \/ InstallRemoteObject
    \/ RejectRemoteAtFullTable
    \/ OpenRemoteObjectPair
    \/ InstallRemoteObjectPair
    \/ RejectRemotePairWithoutCapacity
    \/ RejectMalformedRemote

TypeOK ==
    /\ Tasks = {OwnerMain, OwnerWorker, Foreign, Caller}
    /\ endpointOwnerProcess \in {OwnerProcess, ForeignProcess, NoTask}
    /\ messageState \in {NoMessage, Queued, Received, Replied, Cancelled}
    /\ replyState \in {NoReply, LiveReply, UsedReply}
    /\ replyReceiverProcess \in {OwnerProcess, ForeignProcess, NoTask}
    /\ deliveredTo \in Tasks \cup {NoTask}
    /\ transferState \in {NoTransfer, QueuedTransfer, ReceivedTransfer,
                            InstalledTransfer, DroppedTransfer}
    /\ fdTable \subseteq 3..MaxFd
    /\ fdSnapshot \subseteq 3..MaxFd
    /\ requestedFd \in 3..(MaxFd + 1)
    /\ lastAttempt \in {NoAttempt, ForeignReceive, ForeignReply,
                          ForeignHandleReceive, HugeDup, FullInstall,
                          RemoteFullInstall, RemotePairFullInstall, MalformedRemote}
    /\ lastErrno \in {NoErrno, PermissionErrno, BadDescriptorErrno,
                        CapacityErrno, ProtocolErrno}
    /\ remoteState \in {NoRemote, OpenRemote, InstalledRemote, ClosedRemote,
                           OpenRemotePair, InstalledRemotePair, ClosedRemotePair}
    /\ remoteFdCount \in 0..2

QueuedReplyIsBoundToEndpointOwnerProcess ==
    replyState = LiveReply => replyReceiverProcess = endpointOwnerProcess

DeadEndpointRetainsNoQueuedAuthority ==
    endpointOwnerProcess = NoTask =>
        /\ messageState \notin {Queued, Received}
        /\ replyState # LiveReply
        /\ transferState \in {NoTransfer, DroppedTransfer}

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
        /\ ProcessOf(deliveredTo) = replyReceiverProcess
        /\ IF endpointOwnerProcess = NoTask
              THEN transferState = DroppedTransfer
              ELSE /\ replyReceiverProcess = endpointOwnerProcess
                   /\ transferState = InstalledTransfer

SparseFdRequestDoesNotGrowTable ==
    requestedFd = MaxFd + 1 => fdTable = fdSnapshot

FullDescriptorTableRejectsInstallWithoutMutation ==
    lastAttempt = FullInstall =>
        /\ fdTable = 3..MaxFd
        /\ fdTable = fdSnapshot

TerminalTransferHasNoQueuedAuthority ==
    transferState \in {InstalledTransfer, DroppedTransfer} =>
        messageState # Queued

TerminalMessageHasNoDetachedTransfer ==
    messageState \in {Replied, Cancelled} =>
        transferState \in {InstalledTransfer, DroppedTransfer}

RejectedRemotePublicationRetainsNoAuthority ==
    lastAttempt \in {RemoteFullInstall, RemotePairFullInstall, MalformedRemote} =>
        /\ remoteState \in {ClosedRemote, ClosedRemotePair}
        /\ remoteFdCount = 0
        /\ fdTable = fdSnapshot

InstalledRemoteHasDescriptorAuthority ==
    /\ remoteState = InstalledRemote => remoteFdCount = 1 /\ fdTable # {}
    /\ remoteState = InstalledRemotePair => remoteFdCount = 2 /\ Cardinality(fdTable) >= 2

DescriptorCapacityFailureHasExactErrno ==
    lastAttempt \in {FullInstall, RemoteFullInstall, RemotePairFullInstall} =>
        lastErrno = CapacityErrno

SparseDescriptorFailureHasIdentityErrno ==
    lastAttempt = HugeDup => lastErrno = BadDescriptorErrno

=============================================================================
