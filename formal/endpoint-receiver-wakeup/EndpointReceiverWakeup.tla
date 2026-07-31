----------------------- MODULE EndpointReceiverWakeup -----------------------
EXTENDS Naturals

(*******************************************************************************
Models the endpoint receive poll -> scheduler arm -> endpoint waiter publish ->
block handshake.

Concrete owners:
  * kernel/ipc-runtime/src/ipc/mod.rs
    `recv_endpoint_with_sender_and_limits`, `add_endpoint_receiver_waiter`
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

vars == <<pending, receiverState, waiterPublished, received>>

Init ==
    /\ MaxPending \in Nat \ {0}
    /\ pending = 0
    /\ receiverState = Running
    /\ waiterPublished = FALSE
    /\ received = 0

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
    /\ pending = 0
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
    /\ UNCHANGED vars

Next ==
    \/ Enqueue
    \/ Receive
    \/ Arm
    \/ RegisterWithPending
    \/ RegisterWaiter
    \/ CommitBlock
    \/ Terminal

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ pending \in 0..MaxPending
    /\ receiverState \in {Running, Armed, Blocked}
    /\ waiterPublished \in BOOLEAN
    /\ received \in 0..MaxPending

PendingFastPathRetainsNoWaiter ==
    pending > 0 => ~waiterPublished

BlockedReceiverOwnsExactWaiter ==
    receiverState = Blocked => waiterPublished /\ pending = 0

PublishedWaiterNeedsEmptyQueue ==
    waiterPublished => pending = 0

=============================================================================
