----------------------------- MODULE InputReadiness -----------------------------
EXTENDS Naturals, Sequences

(*******************************************************************************
Models the RustOS DVM input readiness handoff.

Concrete owners and source anchors:
  * ring0 bounded ingress and waiter wake:
      kernel/io-manager/src/input/event_queue.rs
  * poll arm/recheck/commit:
      kernel/compat/src/user/syscall/linux/service_ops/poll_epoll.rs
  * input-policy ingestion on a poll probe or authorized read:
      services/inputd/src/main.rs
  * endpoint wait/wake for those request-driven transfers:
      services/inputd/src/main.rs (SYS_RUSTOS_IPC_RECV)

The ownership boundary is deliberately narrow. Ring0 exposes wakeable bounded
ingress, while inputd owns translation and its policy queue. A poll recheck
uses INPUTD_IPC_OP_STATS, which first transfers ingress then reports policy
readiness. That idempotent probe has a finite IPC deadline; an authorized read
uses the same explicit transfer operation when it owns a real DVM ingress
record.

There is intentionally no idle background-drain action. It could consume the
only ring0-observable record after a client armed poll but before it received a
service-owned readiness result, leaving that client asleep.

The model abstracts authorization identity, key translation, and UI rendering;
those are covered by the endpoint/input-revocation models and Rust/KVM tests.
*******************************************************************************)

CONSTANTS MaxIngress, MaxEvents, PollBound

ReaderStates == {"idle", "armed", "recheck", "ready"}
TransferReasons == {"poll-recheck", "authorized-read"}
DvmSource == "dvm"
DvmIngressKinds == {"linux-key", "pointer-packet"}
IngressRecord == [id: 1..MaxEvents, source: {DvmSource}, kind: DvmIngressKinds]
NoDeadline == 0
MaxTime == (MaxEvents + 1) * PollBound

VARIABLES ingress, policyQueue, delivered, readerState, nextEvent,
          pollDeadline, now, transferHistory

vars == <<ingress, policyQueue, delivered, readerState, nextEvent,
          pollDeadline, now, transferHistory>>

SeqSet(sequence) == {sequence[index] : index \in 1..Len(sequence)}

Distinct(sequence) ==
    \A left, right \in 1..Len(sequence): left # right => sequence[left] # sequence[right]

RECURSIVE TransferCount(_)
TransferCount(history) ==
    IF Len(history) = 0
    THEN 0
    ELSE Len(history[1].events) + TransferCount(SubSeq(history, 2, Len(history)))

Init ==
    /\ ingress = <<>>
    /\ policyQueue = <<>>
    /\ delivered = <<>>
    /\ readerState = "idle"
    /\ nextEvent = 1
    /\ pollDeadline = NoDeadline
    /\ now = 0
    /\ transferHistory = <<>>

\* poll(2) first checks service-owned readiness. A policy record that survived
\* a timed-out probe is immediately readable on retry; otherwise ring0 ingress
\* takes the inputd recheck path.
ArmPoll ==
    /\ readerState = "idle"
    /\ now <= MaxTime - PollBound
    /\ readerState' =
        IF Len(policyQueue) > 0 THEN "ready"
        ELSE IF Len(ingress) = 0 THEN "armed" ELSE "recheck"
    /\ pollDeadline' =
        IF Len(policyQueue) > 0 THEN NoDeadline
        ELSE IF Len(ingress) = 0 THEN now + PollBound ELSE now
    /\ UNCHANGED <<ingress, policyQueue, delivered, nextEvent, now, transferHistory>>

\* The DVM relay appends a bounded, source-validated ingress record. An armed
\* waiter becomes eligible for a service readiness recheck in the same step.
ProduceIngress ==
    \E kind \in DvmIngressKinds:
        /\ Len(ingress) < MaxIngress
        /\ nextEvent <= MaxEvents
        /\ ingress' = Append(ingress,
            [id |-> nextEvent, source |-> DvmSource, kind |-> kind])
        /\ nextEvent' = nextEvent + 1
        /\ readerState' = IF readerState = "armed" THEN "recheck" ELSE readerState
        /\ UNCHANGED <<policyQueue, delivered, pollDeadline, now, transferHistory>>

\* The kernel's poll recheck asks inputd for STATS. inputd linearizes the
\* ingress-to-policy transfer before reporting readable; a timeout with no
\* ingress returns the reader to idle instead.
PollRecheck ==
    /\ readerState = "recheck" \/ (readerState = "armed" /\ now = pollDeadline)
    /\ ingress' = <<>>
    /\ policyQueue' = policyQueue \o ingress
    /\ readerState' = IF Len(policyQueue \o ingress) = 0 THEN "idle" ELSE "ready"
    /\ pollDeadline' = NoDeadline
    /\ transferHistory' =
        IF Len(ingress) = 0
        THEN transferHistory
        ELSE Append(transferHistory,
            [reason |-> "poll-recheck", events |-> ingress])
    /\ UNCHANGED <<delivered, nextEvent, now>>

\* The readiness request is bounded. If inputd has not received it by the
\* deadline, poll returns control without moving ingress; a subsequent probe
\* can retry safely because no event was consumed.
ReadinessProbeTimeout ==
    /\ readerState \in {"armed", "recheck"}
    /\ now = pollDeadline
    /\ readerState' = "idle"
    /\ pollDeadline' = NoDeadline
    /\ UNCHANGED <<ingress, policyQueue, delivered, nextEvent, now, transferHistory>>

\* inputd can complete its transfer just as the caller's bounded reply wait
\* expires. The caller receives no stale ready result, but retry must observe
\* the policy-owned record and no source event may be lost.
TransferThenProbeTimeout ==
    /\ readerState = "recheck"
    /\ now = pollDeadline
    /\ Len(ingress) > 0
    /\ ingress' = <<>>
    /\ policyQueue' = policyQueue \o ingress
    /\ readerState' = "idle"
    /\ pollDeadline' = NoDeadline
    /\ transferHistory' = Append(transferHistory,
        [reason |-> "poll-recheck", events |-> ingress])
    /\ UNCHANGED <<delivered, nextEvent, now>>

\* inputd accepts only an authorized read. It drains any ingress not yet
\* observed by poll, then materializes one policy event for the caller. This
\* is the normal direct-read operation after a race with the readiness path.
AuthorizedRead ==
    /\ Len(policyQueue \o ingress) > 0
    /\ ingress' = <<>>
    /\ delivered' = Append(delivered, (policyQueue \o ingress)[1])
    /\ policyQueue' = SubSeq(policyQueue \o ingress, 2, Len(policyQueue \o ingress))
    /\ readerState' =
        IF Len(policyQueue \o ingress) = 1 THEN "idle" ELSE "ready"
    /\ pollDeadline' = NoDeadline
    /\ transferHistory' =
        IF Len(ingress) = 0
        THEN transferHistory
        ELSE Append(transferHistory,
            [reason |-> "authorized-read", events |-> ingress])
    /\ UNCHANGED <<nextEvent, now>>

\* Time cannot advance through an armed poll deadline. TLC must therefore
\* explore a recheck or a bounded probe-timeout resolution before the bound is
\* exceeded.
Tick ==
    /\ now < MaxTime
    /\ (readerState \in {"armed", "recheck"} => now < pollDeadline)
    /\ now' = now + 1
    /\ UNCHANGED <<ingress, policyQueue, delivered, readerState, nextEvent,
                  pollDeadline, transferHistory>>

Next ==
    \/ ArmPoll
    \/ ProduceIngress
    \/ PollRecheck
    \/ ReadinessProbeTimeout
    \/ TransferThenProbeTimeout
    \/ AuthorizedRead
    \/ Tick

ResolvePoll ==
    PollRecheck \/ ReadinessProbeTimeout \/ TransferThenProbeTimeout

TypeOK ==
    /\ ingress \in Seq(IngressRecord)
    /\ policyQueue \in Seq(IngressRecord)
    /\ delivered \in Seq(IngressRecord)
    /\ readerState \in ReaderStates
    /\ nextEvent \in 1..(MaxEvents + 1)
    /\ pollDeadline \in 0..MaxTime
    /\ now \in 0..MaxTime
    /\ transferHistory \in Seq([reason: TransferReasons, events: Seq(IngressRecord)])
    /\ Len(ingress) <= MaxIngress

\* A waiting reader has a finite service-recheck deadline. The Tick action
\* forbids a behavior that crosses it without resolving readiness.
ArmedPollIsBounded ==
    readerState \in {"armed", "recheck"} =>
        /\ pollDeadline >= now
        /\ pollDeadline <= now + PollBound

\* A reader is reported ready only after inputd owns an actual policy record;
\* no ring0-only wake or fabricated STATS reply can make it readable.
ReadyReaderHasReachablePolicy ==
    readerState = "ready" => Len(policyQueue) > 0

\* While a poll is waiting/rechecking, no prior service queue can be hidden.
WaitingReaderHasNoHiddenPolicy ==
    readerState \in {"armed", "recheck"} => Len(policyQueue) = 0

\* Every source record has exactly one owner, and the number of represented
\* records equals the producer sequence. This catches loss, duplication, and
\* stale replay across ring0 ingress, inputd policy, and delivery.
EventOwnershipIsExact ==
    /\ Distinct(ingress \o policyQueue \o delivered)
    /\ nextEvent = Len(ingress \o policyQueue \o delivered) + 1

\* The raw ingress ABI exposes only the two DVM-authenticated wire kinds. No
\* native PS/2, HID, absolute-pointer, or unlabelled source record can enter
\* either ring0's bounded queue or inputd's policy queue.
DvmIngressOnly ==
    \A record \in SeqSet(ingress \o policyQueue \o delivered):
        /\ record.source = DvmSource
        /\ record.kind \in DvmIngressKinds

\* Every ingress transfer is attributed either to the poll STATS recheck or an
\* authorized read. The exact records, not merely a count, must be witnessed
\* and keep their DVM provenance after leaving ring0.
EveryIngressTransferHasTrustedWitness ==
    /\ \A index \in 1..Len(transferHistory):
        /\ transferHistory[index].reason \in TransferReasons
        /\ Len(transferHistory[index].events) > 0
        /\ \A record \in SeqSet(transferHistory[index].events):
            /\ record.source = DvmSource
            /\ record.kind \in DvmIngressKinds
    /\ TransferCount(transferHistory) = Len(policyQueue \o delivered)

\* The timeout field and the Tick guard prove safety, but not that the timer
\* interrupt and inputd recheck are dispatched.  Make both scheduling
\* assumptions explicit, then check that a pending poll cannot persist.
Spec == Init /\ [][Next]_vars /\ WF_vars(Tick) /\ WF_vars(ResolvePoll)

PendingPollEventuallyResolves ==
    readerState \in {"armed", "recheck"} ~>
        readerState \notin {"armed", "recheck"}
================================================================================
