----------------------- MODULE ChildWaitPublication -----------------------
EXTENDS Naturals
(***************************************************************************
Bounded projection of child_wait.rs and park_for_child_event: one old slot is
withdrawn, one new task claims it and publishes its parent, and one producer
publishes persistent child status under PROCESS_TABLE before a two-read scan.
The recheck uses that same table lock, modeled as atomic status operations.
The scheduler's arm/commit wake retention, multiple consumers, signal races,
weak memory and task-id reuse are not represented. Stale hints are harmless;
only missing the new task after a pending recheck is forbidden.
***************************************************************************)
VARIABLES waiter, producer, task, parent, snapshot, status, pending, woke
vars == <<waiter, producer, task, parent, snapshot, status, pending, woke>>
Init == /\ waiter = "old" /\ producer = "start"
        /\ task = 1 /\ parent = 1 /\ snapshot = 0
        /\ status = FALSE /\ pending = FALSE /\ woke = FALSE
Withdraw == /\ waiter = "old" /\ task = 1
            /\ task' = 0 /\ waiter' = "free"
            /\ UNCHANGED <<producer, parent, snapshot, status, pending, woke>>
Claim == /\ waiter = "free" /\ task = 0
         /\ task' = 2 /\ waiter' = "claimed"
         /\ UNCHANGED <<producer, parent, snapshot, status, pending, woke>>
Register == /\ waiter = "claimed"
            /\ parent' = 2 /\ waiter' = "registered"
            /\ UNCHANGED <<producer, task, snapshot, status, pending, woke>>
Recheck == /\ waiter = "registered"
           /\ pending' = ~status /\ waiter' = "checked"
           /\ UNCHANGED <<producer, task, parent, snapshot, status, woke>>
Publish == /\ producer = "start" /\ status' = TRUE /\ producer' = "scan"
           /\ UNCHANGED <<waiter, task, parent, snapshot, pending, woke>>
ReadTask == /\ producer = "scan" /\ snapshot' = task /\ producer' = "parent"
            /\ UNCHANGED <<waiter, task, parent, status, pending, woke>>
ReadParent == /\ producer = "parent"
              /\ woke' = (snapshot = 2 /\ parent = 2)
              /\ producer' = "done"
              /\ UNCHANGED <<waiter, task, parent, snapshot, status, pending>>
Next == Withdraw \/ Claim \/ Register \/ Recheck \/ Publish \/ ReadTask \/ ReadParent \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars
NoLostWake == (waiter = "checked" /\ pending /\ producer = "done") => woke
=============================================================================
