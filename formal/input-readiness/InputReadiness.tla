----------------------------- MODULE InputReadiness -----------------------------
EXTENDS Naturals, Sequences

(*******************************************************************************
Models the RustOS input readiness handoff.

Concrete owners and source anchors:
  * ring0 bounded ingress and waiter wake:
      kernel/io-manager/src/input/event_queue.rs
  * poll arm/recheck/commit:
      kernel/compat/src/user/syscall/linux/service_ops/poll_epoll.rs
  * input-policy ingestion on an authorized client read only:
      services/inputd/src/main.rs

The key boundary is deliberately narrow: ring0 exposes readiness for its
bounded ingress, while inputd owns translation and its policy queue. Therefore
inputd must not asynchronously remove an ingress record before the poll-woken
reader issues its authorized read. Doing so would leave the reader asleep while
the only observable readiness queue was empty.

The model abstracts authorization identity, key translation, and UI rendering;
those are covered by the endpoint/input-revocation models and Rust/KVM tests.
*******************************************************************************)

CONSTANTS MaxIngress, MaxEvents

ReaderStates == {"idle", "armed", "woken"}

VARIABLES ingress, policyQueue, readerState, nextEvent, readHistory

vars == <<ingress, policyQueue, readerState, nextEvent, readHistory>>

Distinct(sequence) ==
    \A left, right \in 1..Len(sequence): left # right => sequence[left] # sequence[right]

Init ==
    /\ ingress = <<>>
    /\ policyQueue = <<>>
    /\ readerState = "idle"
    /\ nextEvent = 1
    /\ readHistory = <<>>

\* poll(2) arms only after checking the ingress queue. If an event already
\* exists it reports readiness instead of sleeping.
ArmPoll ==
    /\ readerState = "idle"
    /\ readerState' = IF Len(ingress) = 0 THEN "armed" ELSE "woken"
    /\ UNCHANGED <<ingress, policyQueue, nextEvent, readHistory>>

\* The DVM relay appends a bounded, source-validated ingress record. An armed
\* reader is made runnable in the same transition.
ProduceIngress ==
    /\ Len(ingress) < MaxIngress
    /\ nextEvent <= MaxEvents
    /\ ingress' = Append(ingress, nextEvent)
    /\ nextEvent' = nextEvent + 1
    /\ readerState' = IF readerState = "armed" THEN "woken" ELSE readerState
    /\ UNCHANGED <<policyQueue, readHistory>>

\* inputd drains ring0 ingress only while serving the already-woken client's
\* read. This is the concrete dispatch_read -> drain_ingest linearization.
ClientRead ==
    /\ readerState = "woken"
    /\ Len(ingress) > 0
    /\ policyQueue' = policyQueue \o ingress
    /\ ingress' = <<>>
    /\ readerState' = "idle"
    /\ readHistory' = Append(readHistory,
        [reason |-> "client-read", wasWoken |-> TRUE, count |-> Len(ingress)])
    /\ UNCHANGED nextEvent

\* inputd policy consumers may deliver records after translation. They do not
\* alter ring0 readiness or create new records.
ConsumePolicyEvent ==
    /\ Len(policyQueue) > 0
    /\ policyQueue' = SubSeq(policyQueue, 2, Len(policyQueue))
    /\ UNCHANGED <<ingress, readerState, nextEvent, readHistory>>

Next ==
    \/ ArmPoll
    \/ ProduceIngress
    \/ ClientRead
    \/ ConsumePolicyEvent

TypeOK ==
    /\ ingress \in Seq(Nat)
    /\ policyQueue \in Seq(Nat)
    /\ readerState \in ReaderStates
    /\ nextEvent \in 1..(MaxEvents + 1)
    /\ readHistory \in Seq([reason: {"client-read"}, wasWoken: BOOLEAN, count: Nat])
    /\ Len(ingress) <= MaxIngress

\* An armed poll cannot hide queued ingress. Producer wake and arm's recheck
\* both force the state to woken before inputd may drain the record.
ArmedPollCannotMissIngress ==
    readerState = "armed" => Len(ingress) = 0

\* Every removal from the ring0 ingress has an explicit, already-woken client
\* read witness. There is no periodic/eager inputd drain action in Next.
EveryIngressDrainHasWokenClientRead ==
    \A index \in 1..Len(readHistory):
        /\ readHistory[index].reason = "client-read"
        /\ readHistory[index].wasWoken
        /\ readHistory[index].count > 0

\* Each source event has exactly one owner: ring0 ingress or inputd policy
\* queue. Neither an eager service turn nor a repeated read can duplicate it.
IngressAndPolicyRecordsAreUnique ==
    Distinct(ingress \o policyQueue)

NoPolicyRecordWithoutClientRead ==
    Len(policyQueue) > 0 => Len(readHistory) > 0

Spec == Init /\ [][Next]_vars
================================================================================
