----------------------- MODULE EndpointReceiverWakeup -----------------------
EXTENDS Naturals

(*******************************************************************************
Models the endpoint receive arm -> receive-or-wait -> block handshake,
including the standalone poll -> arm -> recheck path.

Concrete owners:
  * kernel/ipc-runtime/src/ipc/endpoint_receive.rs
    `receive_or_wait`: one guard consumes a request or publishes a waiter
  * kernel/ipc-runtime/src/ipc/mod.rs
    standalone receive and waiter registration wrappers
  * kernel/compat/src/user/syscall/linux/ipc_ops.rs
    blocking receive syscall loops
  * kernel/ps/src/multitask/scheduler.rs
    arm/commit/wake task transitions

The endpoint slot is the linearization owner for pending-message observation
and waiter publication. If a producer won the race and a message is already
pending, registration returns the fast-path result without publishing a
waiter. Otherwise a later producer may consume stale receiver authority, wake
a task that never blocked, and inject a false synchronous handoff.
*******************************************************************************)

CONSTANT MaxPending

Running == "running"
Armed == "armed"
Blocked == "blocked"

VARIABLES pending, receiverState, waiterPublished, received

baseVars == <<pending, receiverState, waiterPublished, received>>

\* An exact fast-frame pin must not exclude a second authenticated worker
\* from queued work or waiter publication. Capacity refusal retains both the
\* queued request and caller-owned handle custody for retry/cancellation.
VARIABLES fastState, fastTakenBy, foreignResult, handleRequest, handleOwner
isolationVars == <<fastState, fastTakenBy, foreignResult, handleRequest, handleOwner>>
VARIABLES declaredCapacity, shapeCall, shapeQueuedHandles
shapeVars == <<declaredCapacity, shapeCall, shapeQueuedHandles>>
vars == <<baseVars, isolationVars, shapeVars>>

Init ==
    /\ MaxPending \in Nat \ {0}
    /\ pending = 0
    /\ receiverState = Running
    /\ waiterPublished = FALSE
    /\ received = 0
    /\ fastState = "absent"
    /\ fastTakenBy = 0
    /\ foreignResult = "none"
    /\ handleRequest = "queued"
    /\ handleOwner = "caller"
    /\ declaredCapacity \in 0..1
    /\ shapeCall = "idle"
    /\ shapeQueuedHandles = 0

Enqueue ==
    /\ pending + received < MaxPending
    /\ pending' = pending + 1
    /\ IF waiterPublished
          THEN /\ waiterPublished' = FALSE
               /\ receiverState' = Running
          ELSE /\ waiterPublished' = waiterPublished
               /\ receiverState' = receiverState
    /\ UNCHANGED received

Receive ==
    /\ receiverState = Running
    /\ pending > 0
    /\ pending' = pending - 1
    /\ received' = received + 1
    /\ UNCHANGED <<receiverState, waiterPublished>>

Arm ==
    /\ receiverState = Running
    /\ receiverState' = Armed
    /\ UNCHANGED <<pending, waiterPublished, received>>

\* A producer queued after the initial poll but before endpoint-slot
\* registration. The receiver re-polls without retaining wake authority.
RegisterWithPending ==
    /\ receiverState = Armed
    /\ pending > 0
    /\ receiverState' = Running
    /\ waiterPublished' = FALSE
    /\ UNCHANGED <<pending, received>>

\* Atomic composition of RegisterWithPending and Receive: the combined
\* endpoint transaction consumes the request; compat then cancels its arm.
ReceiveArmed ==
    /\ receiverState = Armed
    /\ pending > 0
    /\ pending' = pending - 1
    /\ received' = received + 1
    /\ receiverState' = Running
    /\ waiterPublished' = FALSE

RegisterWaiter ==
    /\ receiverState = Armed
    /\ pending = 0
    /\ waiterPublished' = TRUE
    /\ UNCHANGED <<pending, receiverState, received>>

CommitBlock ==
    /\ receiverState = Armed
    /\ waiterPublished
    /\ pending = 0
    /\ receiverState' = Blocked
    /\ UNCHANGED <<pending, waiterPublished, received>>

Terminal ==
    /\ received = MaxPending
    /\ UNCHANGED baseVars

PublishFast ==
    /\ fastState = "absent"
    /\ fastState' = "ready"
    /\ UNCHANGED <<fastTakenBy, foreignResult, handleRequest, handleOwner>>

ForeignReceive ==
    /\ fastState = "ready"
    /\ foreignResult = "none"
    /\ foreignResult' \in {"queued", "waiting"}
    /\ UNCHANGED <<fastState, fastTakenBy, handleRequest, handleOwner>>

ExactFastReceive ==
    /\ fastState = "ready"
    /\ fastState' = "taken"
    /\ fastTakenBy' = 1
    /\ UNCHANGED <<foreignResult, handleRequest, handleOwner>>

RefuseHandleCapacity ==
    /\ handleRequest = "queued"
    /\ handleRequest' = "refused"
    /\ UNCHANGED <<fastState, fastTakenBy, foreignResult, handleOwner>>

RetryWithHandleCapacity ==
    /\ handleRequest \in {"queued", "refused"}
    /\ handleRequest' = "taken"
    /\ handleOwner' = "receiver"
    /\ UNCHANGED <<fastState, fastTakenBy, foreignResult>>

CancelHandleRequest ==
    /\ handleRequest \in {"queued", "refused"}
    /\ handleRequest' = "cancelled"
    /\ UNCHANGED <<fastState, fastTakenBy, foreignResult, handleOwner>>

TryDeclaredShape ==
    /\ shapeCall = "idle"
    /\ shapeCall' = IF declaredCapacity = 0 THEN "rejected" ELSE "admitted"
    /\ shapeQueuedHandles' = IF declaredCapacity = 0 THEN 0 ELSE 1
    /\ UNCHANGED declaredCapacity

LegitimateByteCall ==
    /\ shapeCall \in {"rejected", "admitted"}
    /\ shapeCall' = "byte-delivered"
    /\ shapeQueuedHandles' = 0
    /\ UNCHANGED declaredCapacity

Next ==
    \/ /\ (Enqueue \/ Receive \/ Arm \/ RegisterWithPending \/ ReceiveArmed
             \/ RegisterWaiter \/ CommitBlock \/ Terminal)
       /\ UNCHANGED <<isolationVars, shapeVars>>
    \/ /\ (PublishFast \/ ForeignReceive \/ ExactFastReceive
             \/ RefuseHandleCapacity \/ RetryWithHandleCapacity \/ CancelHandleRequest)
       /\ UNCHANGED <<baseVars, shapeVars>>
    \/ /\ (TryDeclaredShape \/ LegitimateByteCall)
       /\ UNCHANGED <<baseVars, isolationVars>>

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ pending \in 0..MaxPending
    /\ receiverState \in {Running, Armed, Blocked}
    /\ waiterPublished \in BOOLEAN
    /\ received \in 0..MaxPending
    /\ fastState \in {"absent", "ready", "taken"}
    /\ fastTakenBy \in {0, 1}
    /\ foreignResult \in {"none", "queued", "waiting"}
    /\ handleRequest \in {"queued", "refused", "taken", "cancelled"}
    /\ handleOwner \in {"caller", "receiver"}
    /\ declaredCapacity \in 0..1
    /\ shapeCall \in {"idle", "rejected", "admitted", "byte-delivered"}
    /\ shapeQueuedHandles \in 0..1

DeclaredShapeBoundsAdmission == shapeQueuedHandles <= declaredCapacity
RejectedShapePublishesNoHandles == shapeCall = "rejected" => shapeQueuedHandles = 0

OnlyExactReceiverTakesFast ==
    (fastState = "taken") <=> (fastTakenBy = 1)

HandleRefusalRetainsRequestAndCustody ==
    handleRequest \in {"queued", "refused", "cancelled"} => handleOwner = "caller"

AcceptedHandlesHaveOneReceiver ==
    (handleRequest = "taken") <=> (handleOwner = "receiver")

PendingFastPathRetainsNoWaiter ==
    pending > 0 => ~waiterPublished

BlockedReceiverOwnsExactWaiter ==
    receiverState = Blocked => waiterPublished /\ pending = 0

PublishedWaiterNeedsEmptyQueue ==
    waiterPublished => pending = 0

=============================================================================
