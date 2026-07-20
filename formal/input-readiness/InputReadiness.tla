----------------------------- MODULE InputReadiness -----------------------------
EXTENDS Naturals, Sequences

(*******************************************************************************
Models the RustOS DVM input readiness handoff.

Concrete owners and source anchors:
  * ring0 bounded ingress and waiter wake:
      kernel/io-manager/src/input/{event_queue.rs,dvm_ring.rs}
  * poll arm/recheck/commit:
      kernel/compat/src/user/syscall/linux/service_ops/poll_epoll.rs
  * event-driven input-policy ingestion plus poll/read refresh:
      services/inputd/src/main.rs
  * endpoint wait/wake for those transfers:
      services/inputd/src/main.rs (SYS_RUSTOS_IPC_RECV)

The ownership boundary is deliberately narrow. Ring0 exposes wakeable bounded
ingress, while inputd owns translation and its policy queue. Its MSI-X-woken
worker can transfer ingress independently of an application. A finite poll
rechecks the 16 ms-bounded INPUTD_IPC_OP_STATS request until its own deadline,
so a record already moved to the service queue is still observed. The kernel
poll path can wake or ask that question but cannot consume a ring slot.

The latency-sensitive uiserver path uses an already-nonblocking native input fd
on a fixed cumulative cadence until inputd exports a service-owned readiness
object. Every read is preceded by the same bounded, non-consuming STATS
recheck used by finite poll. A readiness-gated read can then consume either
service policy or ingress that raced the worker, without starting the
stateful authorize/read transaction merely to discover an empty queue.

The concrete arm/recheck path reads both decoded ingress and the raw fixed-ring
producer/consumer state. This implements the model's `ArmPoll` observation of
`ingress`: a producer edge racing between STATS and waiter registration cannot
be lost merely because no waiter existed at interrupt time.
Finite polls advance through one-tick RTC rechecks until `pollDeadline` rather
than registering one task in independent input and timer waiter tables;
the general indefinite-poll service-readiness object remains an explicit
next-ABI gate and is not claimed by this finite-reader model.

The model abstracts descriptor identity, key translation, and UI rendering;
those are covered by the endpoint/input-revocation models and Rust/KVM tests.
*******************************************************************************)

CONSTANTS MaxIngress, MaxEvents, PollBound

ReaderStates == {"idle", "armed", "recheck", "ready"}
TransferReasons == {"ingestion-worker", "poll-recheck", "readiness-gated-read"}
NoOwner == "none"
InputdBrokerOwner == "inputd-broker"
DvmSource == "dvm"
DvmIngressKinds == {"linux-key", "pointer-packet"}
IngressRecord == [id: 1..MaxEvents, source: {DvmSource}, kind: DvmIngressKinds]
NoDeadline == 0
MaxTime == (MaxEvents + 1) * PollBound

VARIABLES ingress, policyQueue, delivered, readerState, readPermit, nextEvent,
          pollDeadline, now, transferHistory, lastTransferOwner,
          unprivilegedPollAttempted

vars == <<ingress, policyQueue, delivered, readerState, readPermit, nextEvent,
          pollDeadline, now, transferHistory, lastTransferOwner,
          unprivilegedPollAttempted>>

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
    /\ readPermit = FALSE
    /\ nextEvent = 1
    /\ pollDeadline = NoDeadline
    /\ now = 0
    /\ transferHistory = <<>>
    /\ lastTransferOwner = NoOwner
    /\ unprivilegedPollAttempted = FALSE

\* poll(2) first checks service-owned readiness. A policy record that survived
\* a timed-out probe is immediately readable on retry; otherwise ring0 ingress
\* takes the inputd recheck path.
ArmPoll ==
    /\ readerState = "idle"
    /\ (Len(policyQueue \o ingress) > 0 \/ now <= MaxTime - PollBound)
    /\ readerState' =
        IF Len(policyQueue) > 0 THEN "ready"
        ELSE IF Len(ingress) = 0 THEN "armed" ELSE "recheck"
    /\ readPermit' = (Len(policyQueue) > 0)
    /\ pollDeadline' =
        IF Len(policyQueue) > 0 THEN NoDeadline
        ELSE IF Len(ingress) = 0 THEN now + PollBound ELSE now
    /\ UNCHANGED <<ingress, policyQueue, delivered, nextEvent, now, transferHistory,
                  lastTransferOwner, unprivilegedPollAttempted>>

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
        /\ UNCHANGED <<policyQueue, delivered, readPermit, pollDeadline, now, transferHistory,
                      lastTransferOwner, unprivilegedPollAttempted>>

\* inputd's dedicated MSI-X-woken worker owns transport progress even when no
\* application is polling. Moving a record here deliberately does not invent a
\* kernel poll wake: finite poll and uiserver readiness-gated paths recheck
\* service policy.
BackgroundTransfer ==
    /\ Len(ingress) > 0
    /\ ingress' = <<>>
    /\ policyQueue' = policyQueue \o ingress
    /\ transferHistory' = Append(transferHistory,
        [reason |-> "ingestion-worker", events |-> ingress])
    /\ lastTransferOwner' = InputdBrokerOwner
    /\ UNCHANGED <<delivered, readerState, readPermit, nextEvent, pollDeadline, now,
                  unprivilegedPollAttempted>>

\* The kernel's poll recheck asks inputd for STATS. inputd linearizes the
\* ingress-to-policy transfer before reporting readable; a timeout with no
\* ingress returns the reader to idle instead.
PollRecheck ==
    /\ readerState = "recheck" \/ (readerState = "armed" /\ now = pollDeadline)
    /\ ingress' = <<>>
    /\ policyQueue' = policyQueue \o ingress
    /\ readerState' = IF Len(policyQueue \o ingress) = 0 THEN "idle" ELSE "ready"
    /\ readPermit' = (Len(policyQueue \o ingress) > 0)
    /\ pollDeadline' = NoDeadline
    /\ transferHistory' =
        IF Len(ingress) = 0
        THEN transferHistory
        ELSE Append(transferHistory,
            [reason |-> "poll-recheck", events |-> ingress])
    /\ lastTransferOwner' = InputdBrokerOwner
    /\ UNCHANGED <<delivered, nextEvent, now, unprivilegedPollAttempted>>

\* The readiness request is bounded. If inputd has not received it by the
\* deadline, poll returns control without moving ingress; a subsequent probe
\* can retry safely because no event was consumed.
ReadinessProbeTimeout ==
    /\ readerState \in {"armed", "recheck"}
    /\ now = pollDeadline
    /\ readerState' = "idle"
    /\ readPermit' = FALSE
    /\ pollDeadline' = NoDeadline
    /\ UNCHANGED <<ingress, policyQueue, delivered, nextEvent, now, transferHistory,
                  lastTransferOwner, unprivilegedPollAttempted>>

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
    /\ readPermit' = FALSE
    /\ pollDeadline' = NoDeadline
    /\ transferHistory' = Append(transferHistory,
        [reason |-> "poll-recheck", events |-> ingress])
    /\ lastTransferOwner' = InputdBrokerOwner
    /\ UNCHANGED <<delivered, nextEvent, now, unprivilegedPollAttempted>>

\* inputd accepts only a readiness-gated read. It drains any ingress not yet
\* observed by poll, then materializes one policy event for the caller. This
\* is the normal read operation after a race with the readiness path.
ReadinessGatedRead ==
    /\ readPermit
    /\ Len(policyQueue \o ingress) > 0
    /\ ingress' = <<>>
    /\ delivered' = Append(delivered, (policyQueue \o ingress)[1])
    /\ policyQueue' = SubSeq(policyQueue \o ingress, 2, Len(policyQueue \o ingress))
    /\ readerState' =
        IF Len(policyQueue \o ingress) = 1 THEN "idle" ELSE "ready"
    /\ readPermit' = (Len(policyQueue \o ingress) > 1)
    /\ pollDeadline' = NoDeadline
    /\ transferHistory' =
        IF Len(ingress) = 0
        THEN transferHistory
        ELSE Append(transferHistory,
            [reason |-> "readiness-gated-read", events |-> ingress])
    /\ lastTransferOwner' = InputdBrokerOwner
    /\ UNCHANGED <<nextEvent, now, unprivilegedPollAttempted>>

\* A non-inputd poll caller can observe a wake but has no ingest capability.
\* Its drain attempt must not consume a ring0 ingress record, change the
\* policy queue, or claim the consumer identity.
UnprivilegedPollDrainAttempt ==
    /\ ~unprivilegedPollAttempted
    /\ unprivilegedPollAttempted' = TRUE
    /\ UNCHANGED <<ingress, policyQueue, delivered, readerState, readPermit, nextEvent,
                  pollDeadline, now, transferHistory, lastTransferOwner>>

\* Time cannot advance through an armed poll deadline. TLC must therefore
\* explore a recheck or a bounded probe-timeout resolution before the bound is
\* exceeded.
Tick ==
    /\ now < MaxTime
    /\ (readerState \in {"armed", "recheck"} => now < pollDeadline)
    /\ now' = now + 1
    /\ UNCHANGED <<ingress, policyQueue, delivered, readerState, readPermit, nextEvent,
                  pollDeadline, transferHistory, lastTransferOwner,
                  unprivilegedPollAttempted>>

Next ==
    \/ ArmPoll
    \/ ProduceIngress
    \/ BackgroundTransfer
    \/ PollRecheck
    \/ ReadinessProbeTimeout
    \/ TransferThenProbeTimeout
    \/ ReadinessGatedRead
    \/ UnprivilegedPollDrainAttempt
    \/ Tick

ResolvePoll ==
    PollRecheck \/ ReadinessProbeTimeout \/ TransferThenProbeTimeout

TypeOK ==
    /\ ingress \in Seq(IngressRecord)
    /\ policyQueue \in Seq(IngressRecord)
    /\ delivered \in Seq(IngressRecord)
    /\ readerState \in ReaderStates
    /\ readPermit \in BOOLEAN
    /\ nextEvent \in 1..(MaxEvents + 1)
    /\ pollDeadline \in 0..MaxTime
    /\ now \in 0..MaxTime
    /\ transferHistory \in Seq([reason: TransferReasons, events: Seq(IngressRecord)])
    /\ lastTransferOwner \in {NoOwner, InputdBrokerOwner}
    /\ unprivilegedPollAttempted \in BOOLEAN
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

\* A finite poll timeout returns `false`, not a read permit.  The reader may
\* invoke the inputd read operation only after a positive readiness result;
\* this excludes the concrete bug where `ready = 0` still triggered an empty
\* service read on every 8 ms timeout.
ReadPermitIsPositiveReadiness ==
    /\ (readPermit => readerState = "ready" /\ Len(policyQueue \o ingress) > 0)
    /\ (readerState = "idle" => ~readPermit)

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

\* Every ingress transfer is attributed to the ingestion worker, poll STATS
\* recheck, or an authorized read. The exact records, not merely a count, must
\* be witnessed and keep their DVM provenance after leaving ring0.
EveryIngressTransferHasTrustedWitness ==
    /\ \A index \in 1..Len(transferHistory):
        /\ transferHistory[index].reason \in TransferReasons
        /\ Len(transferHistory[index].events) > 0
        /\ \A record \in SeqSet(transferHistory[index].events):
            /\ record.source = DvmSource
            /\ record.kind \in DvmIngressKinds
    /\ TransferCount(transferHistory) = Len(policyQueue \o delivered)

OnlyInputdMovesIngress ==
    lastTransferOwner \in {NoOwner, InputdBrokerOwner}

UnprivilegedPollCannotConsume ==
    unprivilegedPollAttempted => lastTransferOwner \in {NoOwner, InputdBrokerOwner}

\* The timeout field and Tick guard prove safety, but not that the timer,
\* inputd recheck, or readiness-gated consumer is dispatched. Make those scheduler
\* obligations explicit.
ConsumeReadyInput == ReadinessGatedRead

Spec == Init /\ [][Next]_vars /\ WF_vars(Tick) /\ WF_vars(ArmPoll) /\ WF_vars(ResolvePoll)
       /\ WF_vars(ConsumeReadyInput)

PendingPollEventuallyResolves ==
    readerState \in {"armed", "recheck"} ~>
        readerState \notin {"armed", "recheck"}

ServicePolicyEventuallyDrains ==
    Len(policyQueue) > 0 ~> Len(policyQueue) = 0
================================================================================
