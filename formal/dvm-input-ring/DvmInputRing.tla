--------------------------- MODULE DvmInputRing ----------------------------
EXTENDS Naturals, Sequences, FiniteSets

(*******************************************************************************
Release model for the sole supported DVM input data plane.

Concrete source anchors:
  * protocol geometry and cache-line cursor separation:
      libs/driver-domain-protocol/src/lib.rs DvmInputRingHeader
  * L0's only producer and the fixed eventfd:
      libs/driver-domain-host/src/lib.rs InputRingSink
  * RustOS peer-0 admission, MSI-X leaf, and bounded broker drain:
      kernel/io-manager/src/input/dvm_ring.rs
  * RDI3 epoch/sequence validation and inputd policy ingress:
      services/inputd/src/dvm_protocol.rs
      services/inputd/src/main.rs

The DVM is deliberately absent from the ring's write authority. It sends
allowlisted events to L0 on the authenticated control channel; L0 alone can
commit a complete RDI3 record and advance producer. RustOS alone advances
consumer. The eventfd interrupt can wake work but cannot inspect a record,
advance a cursor, or make a policy decision.

The model includes hostile stale/malformed DVM attempts, event loss/spurious
wakes, reconnect epochs, bounded normal admission with a cleanup reserve,
bounded broker turns, a permanent fail-closed MSI-X installation reject, and
runtime revoke/recovery with one permanent receiver vector. It is a
protocol/ownership model, not a claim about DMA or cache-coherence hardware;
source fence and mapping checks are required conformance evidence.
*******************************************************************************)

CONSTANTS Epochs, Slots, Callers, MaxSequence, MaxBrokerBatch,
          MaxTamperAttempts, MaxAttachAttempts

NoEpoch == 0
NoRecord == [kind |-> "empty", epoch |-> NoEpoch, seq |-> 0]
NoOwner == "none"
L0Owner == "l0"

Start(epoch) == [kind |-> "start", epoch |-> epoch, seq |-> 0]
Key(epoch, seq) == [kind |-> "key", epoch |-> epoch, seq |-> seq]
End(epoch, seq) == [kind |-> "end", epoch |-> epoch, seq |-> seq]
Malformed == [kind |-> "malformed", epoch |-> NoEpoch, seq |-> 0]

GoodRecords ==
    {Start(epoch) : epoch \in Epochs}
    \cup {Key(epoch, seq) : epoch \in Epochs, seq \in 1..MaxSequence}
    \cup {End(epoch, seq) : epoch \in Epochs, seq \in 1..(MaxSequence + 1)}
AllRecords == GoodRecords \cup {Malformed}
RingSlots == 0..(Slots - 1)
NormalCapacity == Slots - 1

VARIABLES rustosReady, policyConsumerReady, installRejected, msixArmed, msixVectorAllocations,
          attachAttempts,
          authenticated, streamEpoch, l0NextSequence,
          issuedEpochs, producer, consumer, ring, irqPending,
          brokerOwner, activeDrainCallers, brokerRemaining, activeEpoch, expectedSequence,
          committed, delivered, rejected, lastProducerOwner,
          lastConsumerOwner, dvmTamperAttempts

vars == <<rustosReady, policyConsumerReady, installRejected, msixArmed, msixVectorAllocations,
          attachAttempts,
          authenticated, streamEpoch, l0NextSequence,
          issuedEpochs, producer, consumer, ring, irqPending,
          brokerOwner, activeDrainCallers, brokerRemaining, activeEpoch, expectedSequence,
          committed, delivered, rejected, lastProducerOwner,
          lastConsumerOwner, dvmTamperAttempts>>

Outstanding == producer - consumer
Slot(cursor) == cursor % Slots
AllUnique(sequence) ==
    \A left, right \in 1..Len(sequence): left # right => sequence[left] # sequence[right]
SeqToSet(sequence) == {sequence[index] : index \in 1..Len(sequence)}

Init ==
    /\ rustosReady = FALSE
    /\ policyConsumerReady = FALSE
    /\ installRejected = FALSE
    /\ msixArmed = FALSE
    /\ msixVectorAllocations = 0
    /\ attachAttempts = 0
    /\ authenticated = FALSE
    /\ streamEpoch = NoEpoch
    /\ l0NextSequence = 0
    /\ issuedEpochs = {}
    /\ producer = 0
    /\ consumer = 0
    /\ ring = [slot \in RingSlots |-> NoRecord]
    /\ irqPending = FALSE
    /\ brokerOwner = NoOwner
    /\ activeDrainCallers = {}
    /\ brokerRemaining = 0
    /\ activeEpoch = NoEpoch
    /\ expectedSequence = 0
    /\ committed = <<>>
    /\ delivered = <<>>
    /\ rejected = <<>>
    /\ lastProducerOwner = NoOwner
    /\ lastConsumerOwner = NoOwner
    /\ dvmTamperAttempts = 0

\* RustOS validates exact geometry, maps the launch-owned aperture, and arms
\* exactly one MSI-X receiver before it advertises readiness to L0.
InstallRustosReceiver ==
    /\ ~rustosReady
    /\ ~installRejected
    /\ attachAttempts < MaxAttachAttempts
    /\ rustosReady' = TRUE
    /\ policyConsumerReady' = FALSE
    /\ msixArmed' = TRUE
    /\ msixVectorAllocations' =
        IF msixArmed THEN msixVectorAllocations ELSE msixVectorAllocations + 1
    /\ attachAttempts' = attachAttempts + 1
    /\ UNCHANGED <<installRejected, authenticated, streamEpoch, l0NextSequence, issuedEpochs,
                  producer, consumer, ring, irqPending, brokerOwner, activeDrainCallers,
                  brokerRemaining, activeEpoch, expectedSequence, committed,
                  delivered, rejected, lastProducerOwner, lastConsumerOwner,
                  dvmTamperAttempts>>

\* A malformed topology may fail while a permanent MSI vector is being armed.
\* Re-probing cannot safely reclaim that vector, so this boot remains closed
\* rather than leaking finite interrupt capacity through retries.
RejectRustosReceiver ==
    /\ ~rustosReady
    /\ ~installRejected
    /\ ~msixArmed
    /\ attachAttempts < MaxAttachAttempts
    /\ installRejected' = TRUE
    \* An arm failure may happen after vector reservation. The permanent
    \* reject prevents a second reservation even on that partial failure.
    /\ msixVectorAllocations' =
        IF msixVectorAllocations = 0 THEN 1 ELSE msixVectorAllocations
    /\ attachAttempts' = attachAttempts + 1
    /\ UNCHANGED <<rustosReady, policyConsumerReady, msixArmed, authenticated, streamEpoch, l0NextSequence,
                  issuedEpochs, producer, consumer, ring, irqPending,
                  brokerOwner, activeDrainCallers, brokerRemaining, activeEpoch, expectedSequence,
                  committed, delivered, rejected, lastProducerOwner,
                  lastConsumerOwner, dvmTamperAttempts>>

\* A post-install malformed cursor/header revokes this attachment. The next
\* successful InstallRustosReceiver validates a fresh mapping but reuses the
\* original receiver vector instead of consuming another permanent vector.
RuntimeTransportRevoke ==
    /\ rustosReady
    /\ ~installRejected
    /\ rustosReady' = FALSE
    /\ policyConsumerReady' = FALSE
    /\ authenticated' = FALSE
    /\ streamEpoch' = NoEpoch
    /\ l0NextSequence' = 0
    /\ activeEpoch' = NoEpoch
    /\ expectedSequence' = 0
    /\ UNCHANGED <<installRejected, msixArmed, msixVectorAllocations, attachAttempts,
                  issuedEpochs,
                  producer, consumer, ring, irqPending, brokerOwner, activeDrainCallers,
                  brokerRemaining, committed,
                  delivered, rejected, lastProducerOwner, lastConsumerOwner,
                  dvmTamperAttempts>>

\* The bounded attach/recovery budget prevents a missing or malformed provider
\* from repeatedly probing PCI/MMIO from every input wake. This can occur
\* after a prior valid session, so it closes the transport without rewriting
\* historical producer state.
AttachRetryBudgetExhausted ==
    /\ ~rustosReady
    /\ ~installRejected
    /\ attachAttempts = MaxAttachAttempts
    /\ installRejected' = TRUE
    /\ UNCHANGED <<rustosReady, policyConsumerReady, msixArmed, msixVectorAllocations, attachAttempts,
                  authenticated, streamEpoch, l0NextSequence, issuedEpochs,
                  producer, consumer, ring, irqPending, brokerOwner, activeDrainCallers,
                  brokerRemaining, activeEpoch, expectedSequence, committed,
                  delivered, rejected, lastProducerOwner, lastConsumerOwner,
                  dvmTamperAttempts>>

AuthenticateL0 ==
    /\ rustosReady
    /\ ~authenticated
    /\ authenticated' = TRUE
    /\ UNCHANGED <<rustosReady, policyConsumerReady, installRejected, msixArmed, msixVectorAllocations,
                  attachAttempts,
                  streamEpoch, l0NextSequence, issuedEpochs,
                  producer, consumer, ring, irqPending, brokerOwner, activeDrainCallers,
                  brokerRemaining, activeEpoch, expectedSequence, committed,
                  delivered, rejected, lastProducerOwner, lastConsumerOwner,
                  dvmTamperAttempts>>

\* A real RustOS input handle has completed a successful inputd-backed poll
\* query. MSI-X installation alone must never admit a continuous producer.
PolicyConsumerPoll ==
    /\ rustosReady
    /\ ~policyConsumerReady
    /\ policyConsumerReady' = TRUE
    /\ UNCHANGED <<rustosReady, installRejected, msixArmed, msixVectorAllocations,
                  attachAttempts, authenticated, streamEpoch, l0NextSequence,
                  issuedEpochs, producer, consumer, ring, irqPending,
                  brokerOwner, activeDrainCallers, brokerRemaining, activeEpoch, expectedSequence,
                  committed, delivered, rejected, lastProducerOwner,
                  lastConsumerOwner, dvmTamperAttempts>>

\* L0 commits only after the complete fixed record is written. This action is
\* the producer linearization point and reserves one slot for cleanup.
L0Start(epoch) ==
    /\ rustosReady /\ policyConsumerReady /\ authenticated
    /\ streamEpoch = NoEpoch
    /\ epoch \in Epochs \ issuedEpochs
    /\ Outstanding < NormalCapacity
    /\ LET record == Start(epoch) IN
       /\ ring' = [ring EXCEPT ![Slot(producer)] = record]
       /\ producer' = producer + 1
       /\ streamEpoch' = epoch
       /\ l0NextSequence' = 1
       /\ issuedEpochs' = issuedEpochs \cup {epoch}
       /\ committed' = Append(committed, record)
       /\ lastProducerOwner' = L0Owner
    /\ UNCHANGED <<rustosReady, policyConsumerReady, installRejected, msixArmed, msixVectorAllocations,
                  attachAttempts,
                  authenticated, consumer, irqPending,
                  brokerOwner, activeDrainCallers, brokerRemaining, activeEpoch, expectedSequence,
                  delivered, rejected, lastConsumerOwner, dvmTamperAttempts>>

L0Key ==
    /\ rustosReady /\ policyConsumerReady /\ authenticated
    /\ streamEpoch \in Epochs
    /\ l0NextSequence \in 1..MaxSequence
    /\ Outstanding < NormalCapacity
    /\ LET record == Key(streamEpoch, l0NextSequence) IN
       /\ ring' = [ring EXCEPT ![Slot(producer)] = record]
       /\ producer' = producer + 1
       /\ l0NextSequence' = l0NextSequence + 1
       /\ committed' = Append(committed, record)
       /\ lastProducerOwner' = L0Owner
    /\ UNCHANGED <<rustosReady, policyConsumerReady, installRejected, msixArmed, msixVectorAllocations,
                  attachAttempts,
                  authenticated, streamEpoch, issuedEpochs,
                  consumer, irqPending, brokerOwner, activeDrainCallers, brokerRemaining,
                  activeEpoch, expectedSequence, delivered, rejected,
                  lastConsumerOwner, dvmTamperAttempts>>

\* Session end and synthetic releases are cleanup. They may consume the one
\* reserved slot, but never exceed the exact fixed aperture bound.
L0End ==
    /\ rustosReady /\ policyConsumerReady
    /\ streamEpoch \in Epochs
    /\ l0NextSequence \in 1..(MaxSequence + 1)
    /\ Outstanding < Slots
    /\ LET record == End(streamEpoch, l0NextSequence) IN
       /\ ring' = [ring EXCEPT ![Slot(producer)] = record]
       /\ producer' = producer + 1
       /\ streamEpoch' = NoEpoch
       /\ l0NextSequence' = 0
       /\ committed' = Append(committed, record)
       /\ lastProducerOwner' = L0Owner
    /\ UNCHANGED <<rustosReady, policyConsumerReady, installRejected, msixArmed, msixVectorAllocations,
                  attachAttempts,
                  authenticated, issuedEpochs, consumer,
                  irqPending, brokerOwner, activeDrainCallers, brokerRemaining, activeEpoch,
                  expectedSequence, delivered, rejected, lastConsumerOwner,
                  dvmTamperAttempts>>

\* A malformed DVM record is rejected before the input ring. A compromised
\* DVM also has no map/write authority, so its attempted header/slot mutation
\* cannot change any ring or cursor state.
DvmTamperAttempt ==
    /\ dvmTamperAttempts < MaxTamperAttempts
    /\ dvmTamperAttempts' = dvmTamperAttempts + 1
    /\ UNCHANGED <<rustosReady, policyConsumerReady, installRejected, msixArmed, msixVectorAllocations,
                  attachAttempts,
                  authenticated, streamEpoch, l0NextSequence,
                  issuedEpochs, producer, consumer, ring, irqPending,
                  brokerOwner, activeDrainCallers, brokerRemaining, activeEpoch, expectedSequence,
                  committed, delivered, rejected, lastProducerOwner,
                  lastConsumerOwner>>

\* The eventfd is edge-like and may be delayed or spurious. Its ISR owns only
\* this wake bit; broker calls also recheck producer/consumer directly.
SignalInputIrq ==
    /\ irqPending' = TRUE
    /\ UNCHANGED <<rustosReady, policyConsumerReady, installRejected, msixArmed, msixVectorAllocations,
                  attachAttempts,
                  authenticated, streamEpoch, l0NextSequence,
                  issuedEpochs, producer, consumer, ring, brokerOwner, activeDrainCallers,
                  brokerRemaining, activeEpoch, expectedSequence, committed,
                  delivered, rejected, lastProducerOwner, lastConsumerOwner,
                  dvmTamperAttempts>>

BeginBrokerTurn(caller) ==
    /\ caller \in Callers
    /\ rustosReady
    /\ brokerOwner = NoOwner
    /\ consumer < producer
    /\ brokerOwner' = caller
    /\ activeDrainCallers' = activeDrainCallers \cup {caller}
    /\ brokerRemaining' = MaxBrokerBatch
    /\ irqPending' = FALSE
    /\ UNCHANGED <<rustosReady, policyConsumerReady, installRejected, msixArmed, msixVectorAllocations,
                  attachAttempts,
                  authenticated, streamEpoch, l0NextSequence,
                  issuedEpochs, producer, consumer, ring, activeEpoch,
                  expectedSequence, committed, delivered, rejected,
                  lastProducerOwner, lastConsumerOwner, dvmTamperAttempts>>

ValidStart(record) ==
    record \in {Start(epoch) : epoch \in Epochs} /\ record.epoch \in issuedEpochs
ValidKey(record) ==
    /\ record \in {Key(epoch, seq) : epoch \in Epochs, seq \in 1..MaxSequence}
    /\ record.epoch = activeEpoch
    /\ record.seq = expectedSequence + 1
ValidEnd(record) ==
    /\ record \in {End(epoch, seq) : epoch \in Epochs, seq \in 1..(MaxSequence + 1)}
    /\ record.epoch = activeEpoch
    /\ record.seq = expectedSequence + 1

\* The broker consumes at most MaxBrokerBatch slots. It advances consumer for
\* every observed slot, including malformed/stale records, so one hostile slot
\* cannot pin the finite ring. Only exact current-epoch/sequence keys reach
\* inputd; start/end atomically replace/revoke that epoch's policy state.
ConsumeSlot(caller) ==
    /\ caller \in Callers
    /\ brokerOwner = caller
    /\ activeDrainCallers = {caller}
    /\ brokerRemaining > 0
    /\ consumer < producer
    /\ LET record == ring[Slot(consumer)] IN
       /\ consumer' = consumer + 1
       /\ brokerRemaining' = brokerRemaining - 1
       /\ lastConsumerOwner' = caller
       /\ IF ValidStart(record)
             THEN /\ activeEpoch' = record.epoch
                  /\ expectedSequence' = 0
                  /\ delivered' = delivered
                  /\ rejected' = rejected
             ELSE IF ValidKey(record)
                     THEN /\ activeEpoch' = activeEpoch
                          /\ expectedSequence' = record.seq
                          /\ delivered' = Append(delivered, record)
                          /\ rejected' = rejected
                     ELSE IF ValidEnd(record)
                             THEN /\ activeEpoch' = NoEpoch
                                  /\ expectedSequence' = 0
                                  /\ delivered' = delivered
                                  /\ rejected' = rejected
                             ELSE /\ activeEpoch' = activeEpoch
                                  /\ expectedSequence' = expectedSequence
                                  /\ delivered' = delivered
                                  /\ rejected' = Append(rejected, record)
    /\ UNCHANGED <<rustosReady, policyConsumerReady, installRejected, msixArmed, msixVectorAllocations,
                  attachAttempts,
                  authenticated, streamEpoch, l0NextSequence,
                  issuedEpochs, producer, ring, irqPending, brokerOwner, activeDrainCallers,
                  committed, lastProducerOwner, dvmTamperAttempts>>

FinishBrokerTurn(caller) ==
    /\ caller \in Callers
    /\ brokerOwner = caller
    /\ activeDrainCallers = {caller}
    /\ brokerRemaining = 0 \/ consumer = producer
    /\ brokerOwner' = NoOwner
    /\ activeDrainCallers' = {}
    /\ brokerRemaining' = 0
    /\ UNCHANGED <<rustosReady, policyConsumerReady, installRejected, msixArmed, msixVectorAllocations,
                  attachAttempts,
                  authenticated, streamEpoch, l0NextSequence,
                  issuedEpochs, producer, consumer, ring, irqPending,
                  activeEpoch, expectedSequence, committed, delivered,
                  rejected, lastProducerOwner, lastConsumerOwner,
                  dvmTamperAttempts>>

BrokerBegin == \E caller \in Callers: BeginBrokerTurn(caller)
BrokerConsume == \E caller \in Callers: ConsumeSlot(caller)
BrokerFinish == \E caller \in Callers: FinishBrokerTurn(caller)

Next ==
    \/ InstallRustosReceiver
    \/ RejectRustosReceiver
    \/ RuntimeTransportRevoke
    \/ AttachRetryBudgetExhausted
    \/ AuthenticateL0
    \/ PolicyConsumerPoll
    \/ \E epoch \in Epochs: L0Start(epoch)
    \/ L0Key
    \/ L0End
    \/ DvmTamperAttempt
    \/ SignalInputIrq
    \/ BrokerBegin
    \/ BrokerConsume
    \/ BrokerFinish

TypeOK ==
    /\ rustosReady \in BOOLEAN
    /\ policyConsumerReady \in BOOLEAN
    /\ installRejected \in BOOLEAN
    /\ msixArmed \in BOOLEAN
    /\ msixVectorAllocations \in 0..1
    /\ attachAttempts \in 0..MaxAttachAttempts
    /\ authenticated \in BOOLEAN
    /\ streamEpoch \in Epochs \cup {NoEpoch}
    /\ l0NextSequence \in 0..(MaxSequence + 1)
    /\ issuedEpochs \subseteq Epochs
    /\ producer \in Nat /\ consumer \in Nat
    /\ ring \in [RingSlots -> AllRecords \cup {NoRecord}]
    /\ irqPending \in BOOLEAN
    /\ brokerOwner \in {NoOwner} \union Callers
    /\ activeDrainCallers \subseteq Callers
    /\ brokerRemaining \in 0..MaxBrokerBatch
    /\ activeEpoch \in Epochs \cup {NoEpoch}
    /\ expectedSequence \in 0..MaxSequence
    /\ committed \in Seq(GoodRecords)
    /\ delivered \in Seq({Key(epoch, seq) : epoch \in Epochs, seq \in 1..MaxSequence})
    /\ rejected \in Seq(AllRecords)
    /\ lastProducerOwner \in {NoOwner, L0Owner}
    /\ lastConsumerOwner \in {NoOwner} \union Callers
    /\ dvmTamperAttempts \in 0..MaxTamperAttempts

CursorBound ==
    /\ producer >= consumer
    /\ Outstanding <= Slots

EveryCommitRequiresArmedReceiver == producer > 0 => msixArmed

ActiveProducerRequiresLiveConsumer ==
    streamEpoch # NoEpoch => rustosReady /\ policyConsumerReady /\ authenticated

PolicyReadinessCannotOutliveTransport == policyConsumerReady => rustosReady

RejectedTransportCannotReopen == installRejected => ~rustosReady

MsiVectorAllocationIsBounded ==
    /\ msixVectorAllocations <= 1
    /\ msixArmed => msixVectorAllocations = 1

AttachRecoveryIsBounded == attachAttempts <= MaxAttachAttempts

SingleWriterOwnership ==
    /\ lastProducerOwner \in {NoOwner, L0Owner}
    /\ lastConsumerOwner \in {NoOwner} \union Callers

SingleFlightDrainOwnership ==
    /\ Cardinality(activeDrainCallers) <= 1
    /\ brokerOwner = NoOwner => activeDrainCallers = {}
    /\ brokerOwner \in Callers => activeDrainCallers = {brokerOwner}

InterruptCannotConsumeOrDecidePolicy ==
    brokerOwner = NoOwner => brokerRemaining = 0

CommittedRecordsAreUniqueAndExact ==
    /\ AllUnique(committed)
    /\ \A record \in SeqToSet(committed): record \in GoodRecords

DeliveredRecordsAreUniqueValidatedPrefixes ==
    /\ AllUnique(delivered)
    /\ \A record \in SeqToSet(delivered): record \in SeqToSet(committed)
    /\ \A record \in SeqToSet(delivered): record.seq \in 1..MaxSequence

NoStaleRecordReachesInputd ==
    \A record \in SeqToSet(delivered):
        \E epoch \in Epochs: record.epoch = epoch /\ record \in {Key(epoch, seq) : seq \in 1..MaxSequence}

MalformedDoesNotCreateAuthority ==
    \A record \in SeqToSet(rejected): record = Malformed \/ record \in GoodRecords

Spec ==
    Init /\ [][Next]_vars
    /\ WF_vars(L0End)
    /\ WF_vars(BrokerBegin)
    /\ WF_vars(BrokerConsume)
    /\ WF_vars(BrokerFinish)

RingEventuallyDrainsOrRevokes ==
    []((rustosReady /\ consumer < producer) => <>(consumer = producer \/ ~rustosReady))
=============================================================================
