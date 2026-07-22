---------------------------- MODULE IpcHandleTransfer ----------------------------
EXTENDS Naturals

(*******************************************************************************
Models the lifetime of a batch of opaque IPC-transferred file descriptors.

Concrete owners:
  * kernel/ps/src/user/handles.rs
  * kernel/ipc-runtime/src/ipc/mod.rs
  * kernel/compat/src/user/syscall/linux/ipc_ops.rs
  * kernel/ps/src/multitask/current.rs

The descriptor registry owns the duplicated handle entry until exactly one of:
the receiving process installs it, an enqueue/dequeue/peer-close/caller-exit
cleanup discards it.  The IPC runtime intentionally carries only opaque
KernelTransferredHandle values, while the process substrate owns the actual
handle entries.  This separation is safe only if every path that removes an
endpoint message returns its descriptors to the substrate.

Linearization points: registry insertion, endpoint enqueue/dequeue, pre-dequeue
receiver-output validation, registry
take for installation, registry drop for cancellation/rejection, and caller
observation of a PeerClosed response.

The finite model does not claim the still-unimplemented post-validation
user-mapping race between descriptor installation and numeric-FD copyout; that
transactional reservation/commit gap is tracked as a failed coverage gate.
*******************************************************************************)

CONSTANTS Descriptors

Source == "source"
Exported == "exported"
Queued == "queued"
Received == "received"
Installed == "installed"
Dropped == "dropped"

NoMessage == "none"
QueuedMessage == "queued"
ReceivedMessage == "received"
PeerClosedMessage == "peer-closed"
CancelledMessage == "cancelled"

OutputReady == "ready"
OutputInvalid == "invalid"

VARIABLES transferState,
          registryPresent,
          messageState,
          receiverOutput

vars == <<transferState, registryPresent, messageState, receiverOutput>>

Init ==
    /\ transferState = [descriptor \in Descriptors |-> Source]
    /\ registryPresent = [descriptor \in Descriptors |-> FALSE]
    /\ messageState = NoMessage
    /\ receiverOutput = OutputReady

ExportBatch ==
    /\ \A descriptor \in Descriptors : transferState[descriptor] = Source
    /\ transferState' = [descriptor \in Descriptors |-> Exported]
    /\ registryPresent' = [descriptor \in Descriptors |-> TRUE]
    /\ UNCHANGED <<messageState, receiverOutput>>

EnqueueBatch ==
    /\ \A descriptor \in Descriptors :
        /\ transferState[descriptor] = Exported
        /\ registryPresent[descriptor]
    /\ messageState = NoMessage
    /\ transferState' = [descriptor \in Descriptors |-> Queued]
    /\ messageState' = QueuedMessage
    /\ UNCHANGED <<registryPresent, receiverOutput>>

RejectEnqueue ==
    /\ \A descriptor \in Descriptors : transferState[descriptor] = Exported
    /\ transferState' = [descriptor \in Descriptors |-> Dropped]
    /\ registryPresent' = [descriptor \in Descriptors |-> FALSE]
    /\ messageState' = CancelledMessage
    /\ UNCHANGED receiverOutput

SetInvalidReceiverOutput ==
    /\ messageState = QueuedMessage
    /\ receiverOutput = OutputReady
    /\ receiverOutput' = OutputInvalid
    /\ UNCHANGED <<transferState, registryPresent, messageState>>

(*******************************************************************************
The runtime's capacity check leaves the message queued. If a later user-output
validation fails after dequeue, compat must return the moved descriptors to the
registry owner rather than leak them in a detached message.
*******************************************************************************)
RejectReceivedBatch ==
    /\ messageState = QueuedMessage
    /\ receiverOutput = OutputInvalid
    /\ \A descriptor \in Descriptors : transferState[descriptor] = Queued
    /\ transferState' = [descriptor \in Descriptors |-> Dropped]
    /\ registryPresent' = [descriptor \in Descriptors |-> FALSE]
    /\ messageState' = CancelledMessage
    /\ UNCHANGED receiverOutput

ReceiveBatch ==
    /\ messageState = QueuedMessage
    /\ receiverOutput = OutputReady
    /\ \A descriptor \in Descriptors :
        /\ transferState[descriptor] = Queued
        /\ registryPresent[descriptor]
    /\ transferState' = [descriptor \in Descriptors |-> Received]
    /\ messageState' = ReceivedMessage
    /\ UNCHANGED <<registryPresent, receiverOutput>>

InstallBatch ==
    /\ messageState = ReceivedMessage
    /\ \A descriptor \in Descriptors :
        /\ transferState[descriptor] = Received
        /\ registryPresent[descriptor]
    /\ transferState' = [descriptor \in Descriptors |-> Installed]
    /\ registryPresent' = [descriptor \in Descriptors |-> FALSE]
    /\ messageState' = NoMessage
    /\ UNCHANGED receiverOutput

\* A peer can die after dequeue but before the process substrate installs the
\* descriptors.  This is distinct from a queued-message close: the registry
\* is still the sole owner, so teardown must explicitly drop the received
\* batch instead of leaving it detached from both endpoint and fd table.
EndpointOwnerExitsWithReceivedBatch ==
    /\ messageState = ReceivedMessage
    /\ \A descriptor \in Descriptors :
        /\ transferState[descriptor] = Received
        /\ registryPresent[descriptor]
    /\ transferState' = [descriptor \in Descriptors |-> Dropped]
    /\ registryPresent' = [descriptor \in Descriptors |-> FALSE]
    /\ messageState' = CancelledMessage
    /\ UNCHANGED receiverOutput

CallerCancelsQueuedBatch ==
    /\ messageState \in {QueuedMessage, PeerClosedMessage}
    /\ \A descriptor \in Descriptors : transferState[descriptor] = Queued
    /\ transferState' = [descriptor \in Descriptors |-> Dropped]
    /\ registryPresent' = [descriptor \in Descriptors |-> FALSE]
    /\ messageState' = CancelledMessage
    /\ UNCHANGED receiverOutput

EndpointOwnerCloses ==
    /\ messageState = QueuedMessage
    /\ \A descriptor \in Descriptors : transferState[descriptor] = Queued
    /\ messageState' = PeerClosedMessage
    /\ UNCHANGED <<transferState, registryPresent, receiverOutput>>

CallerObservesPeerClose ==
    /\ messageState = PeerClosedMessage
    /\ \A descriptor \in Descriptors : transferState[descriptor] = Queued
    /\ transferState' = [descriptor \in Descriptors |-> Dropped]
    /\ registryPresent' = [descriptor \in Descriptors |-> FALSE]
    /\ messageState' = CancelledMessage
    /\ UNCHANGED receiverOutput

(*******************************************************************************
Task retirement cancels its outstanding calls before endpoint teardown.  This
also collects a response-handle batch that arrived after the caller stopped
waiting, so neither direction can pin the registry after caller exit.
*******************************************************************************)
CallerExitsWithQueuedBatch ==
    /\ messageState \in {QueuedMessage, PeerClosedMessage}
    /\ \A descriptor \in Descriptors : transferState[descriptor] = Queued
    /\ transferState' = [descriptor \in Descriptors |-> Dropped]
    /\ registryPresent' = [descriptor \in Descriptors |-> FALSE]
    /\ messageState' = CancelledMessage
    /\ UNCHANGED receiverOutput

Next ==
    \/ ExportBatch
    \/ EnqueueBatch
    \/ RejectEnqueue
    \/ SetInvalidReceiverOutput
    \/ RejectReceivedBatch
    \/ ReceiveBatch
    \/ InstallBatch
    \/ EndpointOwnerExitsWithReceivedBatch
    \/ CallerCancelsQueuedBatch
    \/ EndpointOwnerCloses
    \/ CallerObservesPeerClose
    \/ CallerExitsWithQueuedBatch

TypeOK ==
    /\ Descriptors \subseteq Nat
    /\ transferState \in [Descriptors -> {Source, Exported, Queued, Received,
                                           Installed, Dropped}]
    /\ registryPresent \in [Descriptors -> BOOLEAN]
    /\ messageState \in {NoMessage, QueuedMessage, ReceivedMessage,
                          PeerClosedMessage, CancelledMessage}
    /\ receiverOutput \in {OutputReady, OutputInvalid}

RegistryContainsExactlyLiveDescriptors ==
    \A descriptor \in Descriptors :
        registryPresent[descriptor] <=>
            transferState[descriptor] \in {Exported, Queued, Received}

BatchTransferIsAllOrNothing ==
    \A first, second \in Descriptors :
        transferState[first] = transferState[second]

QueuedMessageHasExactlyOneRegistryBatch ==
    messageState \in {QueuedMessage, PeerClosedMessage} =>
        \A descriptor \in Descriptors :
            /\ transferState[descriptor] = Queued
            /\ registryPresent[descriptor]

ReceivedMessageHasExactlyOneRegistryBatch ==
    messageState = ReceivedMessage =>
        \A descriptor \in Descriptors :
            /\ transferState[descriptor] = Received
            /\ registryPresent[descriptor]

TerminalMessageCannotPinReceivedDescriptors ==
    messageState \in {NoMessage, CancelledMessage} =>
        \A descriptor \in Descriptors:
            transferState[descriptor] \notin {Queued, Received}

InvalidReceiverOutputNeverInstallsDescriptors ==
    receiverOutput = OutputInvalid =>
        \A descriptor \in Descriptors : transferState[descriptor] # Installed

TerminalDescriptorsCarryNoRegistryAuthority ==
    \A descriptor \in Descriptors :
        transferState[descriptor] \in {Installed, Dropped} =>
            /\ ~registryPresent[descriptor]
            /\ messageState # QueuedMessage
            /\ messageState # ReceivedMessage
            /\ messageState # PeerClosedMessage

CancelledMessageHasNoLiveDescriptor ==
    messageState = CancelledMessage =>
        \A descriptor \in Descriptors : transferState[descriptor] = Dropped

=============================================================================
