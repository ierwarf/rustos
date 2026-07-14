------------------------------- MODULE DvmNetworkRing ----------------------------
EXTENDS Naturals, FiniteSets

(*******************************************************************************
Models the fixed two-ring ivshmem Ethernet substrate.

Concrete owners and source anchors:
  * protocol header: libs/driver-domain-protocol/src/lib.rs
  * RustOS ring mapper/consumer: kernel/io-manager/src/io/dvm_network.rs
  * Linux DVM relay: driver-domains/linux/package/rustos-dvm-net/
    src/rustos-dvm-net.c

The host initializes the fixed header before either guest starts. RustOS copies
and validates that header once while it installs the aperture. Thereafter the
DVM may control shared counters and payload bytes, but never a pointer,
descriptor, allocation size, slot count, or kernel-owned consumer cursor.
Every producer/consumer distance must be within the fixed ring capacity before
the receiver or transmitter advances a kernel cursor. A malformed DVM receive
slot or forged producer is rejected without delivery or consumer advancement.

This bounded model abstracts u32 modular arithmetic to non-wrapping counters;
the implementation uses wrapping subtraction plus the same <= slot-count
predicate. It also abstracts Ethernet bytes to valid/malformed frame tags.
*******************************************************************************)

CONSTANTS Slots, MaxFrameLength, MaxCounter, MaxRejections

NoHeader == "none"
ValidHeader == "valid"
TamperedHeader == "tampered"
None == "none"
TxRejected == "tx-rejected"
RxRejected == "rx-rejected"
RxDelivered == "rx-delivered"

Counter == 0..MaxCounter
Sequences == 1..MaxCounter

WithinRing(producer, consumer) ==
    /\ producer >= consumer
    /\ producer - consumer <= Slots

VARIABLES rawHeader,
          installedHeader,
          installed,
          txProducer,
          dvmTxConsumer,
          rxProducer,
          kernelRxConsumer,
          txPublished,
          validRx,
          deliveredRx,
          rejectedTx,
          rejectedRx,
          lastOutcome

vars == <<rawHeader, installedHeader, installed, txProducer, dvmTxConsumer,
          rxProducer, kernelRxConsumer, txPublished, validRx, deliveredRx,
          rejectedTx, rejectedRx, lastOutcome>>

Init ==
    /\ rawHeader = ValidHeader
    /\ installedHeader = NoHeader
    /\ installed = FALSE
    /\ txProducer = 0
    /\ dvmTxConsumer = 0
    /\ rxProducer = 0
    /\ kernelRxConsumer = 0
    /\ txPublished = {}
    /\ validRx = {}
    /\ deliveredRx = {}
    /\ rejectedTx = 0
    /\ rejectedRx = 0
    /\ lastOutcome = None

Install ==
    /\ ~installed
    /\ rawHeader = ValidHeader
    /\ installed' = TRUE
    /\ installedHeader' = ValidHeader
    /\ UNCHANGED <<rawHeader, txProducer, dvmTxConsumer, rxProducer,
                  kernelRxConsumer, txPublished, validRx, deliveredRx,
                  rejectedTx, rejectedRx, lastOutcome>>

TamperRawHeader ==
    /\ installed
    /\ rawHeader' = TamperedHeader
    /\ UNCHANGED <<installedHeader, installed, txProducer, dvmTxConsumer,
                  rxProducer, kernelRxConsumer, txPublished, validRx,
                  deliveredRx, rejectedTx, rejectedRx, lastOutcome>>

KernelTransmit ==
    /\ installed
    /\ txProducer < MaxCounter
    /\ WithinRing(txProducer, dvmTxConsumer)
    /\ txProducer - dvmTxConsumer < Slots
    /\ txProducer' = txProducer + 1
    /\ txPublished' = txPublished \cup {txProducer + 1}
    /\ lastOutcome' = None
    /\ UNCHANGED <<rawHeader, installedHeader, installed, dvmTxConsumer,
                  rxProducer, kernelRxConsumer, validRx, deliveredRx,
                  rejectedTx, rejectedRx>>

RejectForgedTxConsumer ==
    /\ installed
    /\ ~WithinRing(txProducer, dvmTxConsumer)
    /\ rejectedTx < MaxRejections
    /\ rejectedTx' = rejectedTx + 1
    /\ lastOutcome' = TxRejected
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  dvmTxConsumer, rxProducer, kernelRxConsumer, txPublished,
                  validRx, deliveredRx, rejectedRx>>

DvmConsumeTx ==
    /\ installed
    /\ dvmTxConsumer < txProducer
    /\ dvmTxConsumer' = dvmTxConsumer + 1
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  rxProducer, kernelRxConsumer, txPublished, validRx,
                  deliveredRx, rejectedTx, rejectedRx, lastOutcome>>

ForgeTxConsumer(cursor) ==
    /\ installed
    /\ cursor \in Counter
    /\ ~WithinRing(txProducer, cursor)
    /\ dvmTxConsumer' = cursor
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  rxProducer, kernelRxConsumer, txPublished, validRx,
                  deliveredRx, rejectedTx, rejectedRx, lastOutcome>>

DvmProduceValidRx ==
    /\ installed
    /\ rxProducer < MaxCounter
    /\ WithinRing(rxProducer, kernelRxConsumer)
    /\ rxProducer - kernelRxConsumer < Slots
    /\ rxProducer' = rxProducer + 1
    /\ validRx' = validRx \cup {rxProducer + 1}
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  dvmTxConsumer, kernelRxConsumer, txPublished, deliveredRx,
                  rejectedTx, rejectedRx, lastOutcome>>

DvmProduceMalformedRx ==
    /\ installed
    /\ rxProducer < MaxCounter
    /\ WithinRing(rxProducer, kernelRxConsumer)
    /\ rxProducer - kernelRxConsumer < Slots
    /\ rxProducer' = rxProducer + 1
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  dvmTxConsumer, kernelRxConsumer, txPublished, validRx,
                  deliveredRx, rejectedTx, rejectedRx, lastOutcome>>

ForgeRxProducer(cursor) ==
    /\ installed
    /\ cursor \in Counter
    /\ ~WithinRing(cursor, kernelRxConsumer)
    /\ rxProducer' = cursor
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  dvmTxConsumer, kernelRxConsumer, txPublished, validRx,
                  deliveredRx, rejectedTx, rejectedRx, lastOutcome>>

KernelReceiveValid ==
    /\ installed
    /\ WithinRing(rxProducer, kernelRxConsumer)
    /\ kernelRxConsumer < rxProducer
    /\ kernelRxConsumer + 1 \in validRx
    /\ kernelRxConsumer' = kernelRxConsumer + 1
    /\ deliveredRx' = deliveredRx \cup {kernelRxConsumer + 1}
    /\ lastOutcome' = RxDelivered
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  dvmTxConsumer, rxProducer, txPublished, validRx,
                  rejectedTx, rejectedRx>>

KernelRejectRx ==
    /\ installed
    /\ ( ~WithinRing(rxProducer, kernelRxConsumer)
         \/ (kernelRxConsumer < rxProducer /\ kernelRxConsumer + 1 \notin validRx) )
    /\ rejectedRx < MaxRejections
    /\ rejectedRx' = rejectedRx + 1
    /\ lastOutcome' = RxRejected
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  dvmTxConsumer, rxProducer, kernelRxConsumer, txPublished,
                  validRx, deliveredRx, rejectedTx>>

Next ==
    \/ Install
    \/ TamperRawHeader
    \/ KernelTransmit
    \/ RejectForgedTxConsumer
    \/ DvmConsumeTx
    \/ \E cursor \in Counter : ForgeTxConsumer(cursor)
    \/ DvmProduceValidRx
    \/ DvmProduceMalformedRx
    \/ \E cursor \in Counter : ForgeRxProducer(cursor)
    \/ KernelReceiveValid
    \/ KernelRejectRx

TypeOK ==
    /\ rawHeader \in {ValidHeader, TamperedHeader}
    /\ installedHeader \in {NoHeader, ValidHeader}
    /\ installed \in BOOLEAN
    /\ txProducer \in Counter
    /\ dvmTxConsumer \in Counter
    /\ rxProducer \in Counter
    /\ kernelRxConsumer \in Counter
    /\ txPublished \subseteq Sequences
    /\ validRx \subseteq Sequences
    /\ deliveredRx \subseteq Sequences
    /\ MaxRejections \in Nat
    /\ rejectedTx \in 0..MaxRejections
    /\ rejectedRx \in 0..MaxRejections
    /\ lastOutcome \in {None, TxRejected, RxRejected, RxDelivered}

InstalledTransportHasHostValidatedHeader ==
    installed => installedHeader = ValidHeader

KernelProducerTracksOnlyItsOwnPublishes ==
    txProducer = Cardinality(txPublished)

KernelConsumerTracksOnlyDeliveredRx ==
    kernelRxConsumer = Cardinality(deliveredRx)

OnlyValidatedRxSlotsReachNetworkPolicy ==
    deliveredRx \subseteq validRx

NoDvmControlledHeaderMutationRevokesInstalledBounds ==
    rawHeader = TamperedHeader /\ installed => installedHeader = ValidHeader

=============================================================================
