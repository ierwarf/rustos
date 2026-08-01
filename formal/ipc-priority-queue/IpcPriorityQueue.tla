------------------------------ MODULE IpcPriorityQueue ------------------------------
EXTENDS Naturals, Sequences, FiniteSets

(*******************************************************************************
Models the kernel-derived two-lane endpoint queue in kernel-ipc-runtime. System
membership is immutable input from the scheduler boundary; an endpoint cannot
derive it from untrusted message bytes. FIFO is preserved inside each lane.
System calls may bypass an ordinary backlog, but after MaxSystemBurst System
deliveries one queued ordinary call is mandatory.
*******************************************************************************)

CONSTANTS Messages, SystemMessages, Capacity, MaxSystemBurst

ASSUME /\ SystemMessages \subseteq Messages
       /\ Capacity > 0
       /\ MaxSystemBurst > 0

Free == "free"
Queued == "queued"
Delivered == "delivered"
NoClass == "none"
System == "system"
Ordinary == "ordinary"

OrdinaryMessages == Messages \ SystemMessages

VARIABLES state,
          systemQ,
          ordinaryQ,
          delivered,
          systemStreak,
          lastClass,
          lastSystemEligible,
          lastOrdinaryReserved

vars == <<state, systemQ, ordinaryQ, delivered, systemStreak, lastClass,
          lastSystemEligible, lastOrdinaryReserved>>

SeqSet(seq) == {seq[index] : index \in 1..Len(seq)}
NoDuplicates(seq) == Cardinality(SeqSet(seq)) = Len(seq)
QueueLen == Len(systemQ) + Len(ordinaryQ)

SystemEligible ==
    /\ Len(systemQ) > 0
    /\ (Len(ordinaryQ) = 0 \/ systemStreak < MaxSystemBurst)

OrdinaryReserved ==
    /\ Len(systemQ) > 0
    /\ Len(ordinaryQ) > 0
    /\ systemStreak = MaxSystemBurst

Init ==
    /\ Messages # {}
    /\ state = [message \in Messages |-> Free]
    /\ systemQ = <<>>
    /\ ordinaryQ = <<>>
    /\ delivered = <<>>
    /\ systemStreak = 0
    /\ lastClass = NoClass
    /\ lastSystemEligible = FALSE
    /\ lastOrdinaryReserved = FALSE

Enqueue(message) ==
    /\ message \in Messages
    /\ state[message] = Free
    /\ QueueLen < Capacity
    /\ state' = [state EXCEPT ![message] = Queued]
    /\ IF message \in SystemMessages
          THEN /\ systemQ' = Append(systemQ, message)
               /\ UNCHANGED ordinaryQ
          ELSE /\ ordinaryQ' = Append(ordinaryQ, message)
               /\ UNCHANGED systemQ
    /\ UNCHANGED <<delivered, systemStreak>>
    /\ lastClass' = NoClass
    /\ lastSystemEligible' = FALSE
    /\ lastOrdinaryReserved' = FALSE

DeliverSystem ==
    /\ SystemEligible
    /\ LET message == Head(systemQ) IN
       /\ systemQ' = Tail(systemQ)
       /\ state' = [state EXCEPT ![message] = Delivered]
       /\ delivered' = Append(delivered, message)
    /\ UNCHANGED ordinaryQ
    /\ systemStreak' =
          IF systemStreak < MaxSystemBurst
          THEN systemStreak + 1
          ELSE MaxSystemBurst
    /\ lastClass' = System
    /\ lastSystemEligible' = SystemEligible
    /\ lastOrdinaryReserved' = OrdinaryReserved

DeliverOrdinary ==
    /\ Len(ordinaryQ) > 0
    /\ ~SystemEligible
    /\ LET message == Head(ordinaryQ) IN
       /\ ordinaryQ' = Tail(ordinaryQ)
       /\ state' = [state EXCEPT ![message] = Delivered]
       /\ delivered' = Append(delivered, message)
    /\ UNCHANGED systemQ
    /\ systemStreak' = 0
    /\ lastClass' = Ordinary
    /\ lastSystemEligible' = SystemEligible
    /\ lastOrdinaryReserved' = OrdinaryReserved

TerminalStutter ==
    /\ \A message \in Messages: state[message] = Delivered
    /\ UNCHANGED vars

Next ==
    \/ \E message \in Messages: Enqueue(message)
    \/ DeliverSystem
    \/ DeliverOrdinary
    \/ TerminalStutter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ state \in [Messages -> {Free, Queued, Delivered}]
    /\ systemQ \in Seq(SystemMessages)
    /\ ordinaryQ \in Seq(OrdinaryMessages)
    /\ delivered \in Seq(Messages)
    /\ systemStreak \in 0..MaxSystemBurst
    /\ lastClass \in {NoClass, System, Ordinary}
    /\ lastSystemEligible \in BOOLEAN
    /\ lastOrdinaryReserved \in BOOLEAN

QueueStateIsExact ==
    /\ NoDuplicates(systemQ)
    /\ NoDuplicates(ordinaryQ)
    /\ SeqSet(systemQ) \cap SeqSet(ordinaryQ) = {}
    /\ \A message \in Messages:
          (state[message] = Queued) <=>
              message \in SeqSet(systemQ) \cup SeqSet(ordinaryQ)

QueueCapacityIsGlobal == QueueLen <= Capacity

SystemBacklogCannotBeBypassed ==
    lastClass = Ordinary => ~lastSystemEligible

OrdinaryReservationCannotBeBypassed ==
    lastClass = System => ~lastOrdinaryReserved

=============================================================================
