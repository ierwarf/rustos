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
RustOS may install before the Linux relay maps the BAR, but installation
requires a prefetchable WB atomic-control contract and Linux may publish its
data-plane readiness only after its own exact WB mapping is active.
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
CacheModes == {"wb", "wc"}
CacheContracts ==
    [barPrefetchable : BOOLEAN,
     rustos : CacheModes,
     linux : CacheModes,
     rustosAtomics : BOOLEAN,
     linuxAtomics : BOOLEAN]

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
          lastOutcome,
          cacheContract,
          linuxReady

vars == <<rawHeader, installedHeader, installed, txProducer, dvmTxConsumer,
          rxProducer, kernelRxConsumer, txPublished, validRx, deliveredRx,
          rejectedTx, rejectedRx, lastOutcome, cacheContract, linuxReady>>

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
    /\ cacheContract \in CacheContracts
    /\ linuxReady = FALSE

Install ==
    /\ ~installed
    /\ rawHeader = ValidHeader
    /\ cacheContract.barPrefetchable
    /\ cacheContract.rustos = "wb"
    /\ cacheContract.rustosAtomics
    /\ installed' = TRUE
    /\ installedHeader' = ValidHeader
    /\ UNCHANGED <<rawHeader, txProducer, dvmTxConsumer, rxProducer,
                  kernelRxConsumer, txPublished, validRx, deliveredRx,
                  rejectedTx, rejectedRx, lastOutcome, cacheContract,
                  linuxReady>>

PublishLinuxReady ==
    /\ installed
    /\ ~linuxReady
    /\ cacheContract.linux = "wb"
    /\ cacheContract.linuxAtomics
    /\ linuxReady' = TRUE
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                   dvmTxConsumer, rxProducer, kernelRxConsumer, txPublished,
                   validRx, deliveredRx, rejectedTx, rejectedRx, lastOutcome,
                   cacheContract>>

TamperRawHeader ==
    /\ installed
    /\ linuxReady
    /\ rawHeader' = TamperedHeader
    /\ UNCHANGED <<installedHeader, installed, txProducer, dvmTxConsumer,
                  rxProducer, kernelRxConsumer, txPublished, validRx,
                  deliveredRx, rejectedTx, rejectedRx, lastOutcome,
                  cacheContract, linuxReady>>

KernelTransmit ==
    /\ installed
    /\ linuxReady
    /\ txProducer < MaxCounter
    /\ WithinRing(txProducer, dvmTxConsumer)
    /\ txProducer - dvmTxConsumer < Slots
    /\ txProducer' = txProducer + 1
    /\ txPublished' = txPublished \cup {txProducer + 1}
    /\ lastOutcome' = None
    /\ UNCHANGED <<rawHeader, installedHeader, installed, dvmTxConsumer,
                  rxProducer, kernelRxConsumer, validRx, deliveredRx,
                  rejectedTx, rejectedRx, cacheContract, linuxReady>>

RejectForgedTxConsumer ==
    /\ installed
    /\ linuxReady
    /\ ~WithinRing(txProducer, dvmTxConsumer)
    /\ rejectedTx < MaxRejections
    /\ rejectedTx' = rejectedTx + 1
    /\ lastOutcome' = TxRejected
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  dvmTxConsumer, rxProducer, kernelRxConsumer, txPublished,
                  validRx, deliveredRx, rejectedRx, cacheContract,
                  linuxReady>>

DvmConsumeTx ==
    /\ installed
    /\ linuxReady
    /\ dvmTxConsumer < txProducer
    /\ dvmTxConsumer' = dvmTxConsumer + 1
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  rxProducer, kernelRxConsumer, txPublished, validRx,
                  deliveredRx, rejectedTx, rejectedRx, lastOutcome,
                  cacheContract, linuxReady>>

ForgeTxConsumer(cursor) ==
    /\ installed
    /\ linuxReady
    /\ cursor \in Counter
    /\ ~WithinRing(txProducer, cursor)
    /\ dvmTxConsumer' = cursor
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  rxProducer, kernelRxConsumer, txPublished, validRx,
                  deliveredRx, rejectedTx, rejectedRx, lastOutcome,
                  cacheContract, linuxReady>>

DvmProduceValidRx ==
    /\ installed
    /\ linuxReady
    /\ rxProducer < MaxCounter
    /\ WithinRing(rxProducer, kernelRxConsumer)
    /\ rxProducer - kernelRxConsumer < Slots
    /\ rxProducer' = rxProducer + 1
    /\ validRx' = validRx \cup {rxProducer + 1}
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  dvmTxConsumer, kernelRxConsumer, txPublished, deliveredRx,
                  rejectedTx, rejectedRx, lastOutcome, cacheContract,
                  linuxReady>>

DvmProduceMalformedRx ==
    /\ installed
    /\ linuxReady
    /\ rxProducer < MaxCounter
    /\ WithinRing(rxProducer, kernelRxConsumer)
    /\ rxProducer - kernelRxConsumer < Slots
    /\ rxProducer' = rxProducer + 1
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  dvmTxConsumer, kernelRxConsumer, txPublished, validRx,
                  deliveredRx, rejectedTx, rejectedRx, lastOutcome,
                  cacheContract, linuxReady>>

ForgeRxProducer(cursor) ==
    /\ installed
    /\ linuxReady
    /\ cursor \in Counter
    /\ ~WithinRing(cursor, kernelRxConsumer)
    /\ rxProducer' = cursor
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  dvmTxConsumer, kernelRxConsumer, txPublished, validRx,
                  deliveredRx, rejectedTx, rejectedRx, lastOutcome,
                  cacheContract, linuxReady>>

KernelReceiveValid ==
    /\ installed
    /\ linuxReady
    /\ WithinRing(rxProducer, kernelRxConsumer)
    /\ kernelRxConsumer < rxProducer
    /\ kernelRxConsumer + 1 \in validRx
    /\ kernelRxConsumer' = kernelRxConsumer + 1
    /\ deliveredRx' = deliveredRx \cup {kernelRxConsumer + 1}
    /\ lastOutcome' = RxDelivered
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  dvmTxConsumer, rxProducer, txPublished, validRx,
                  rejectedTx, rejectedRx, cacheContract, linuxReady>>

KernelRejectRx ==
    /\ installed
    /\ linuxReady
    /\ ( ~WithinRing(rxProducer, kernelRxConsumer)
         \/ (kernelRxConsumer < rxProducer /\ kernelRxConsumer + 1 \notin validRx) )
    /\ rejectedRx < MaxRejections
    /\ rejectedRx' = rejectedRx + 1
    /\ lastOutcome' = RxRejected
    /\ UNCHANGED <<rawHeader, installedHeader, installed, txProducer,
                  dvmTxConsumer, rxProducer, kernelRxConsumer, txPublished,
                  validRx, deliveredRx, rejectedTx, cacheContract,
                  linuxReady>>

Next ==
    \/ Install
    \/ PublishLinuxReady
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
    /\ cacheContract \in CacheContracts
    /\ linuxReady \in BOOLEAN

InstalledTransportHasHostValidatedHeader ==
    installed => installedHeader = ValidHeader

InstalledTransportHasPrefetchableRustosWbAtomics ==
    installed =>
        /\ cacheContract.barPrefetchable
        /\ cacheContract.rustos = "wb"
        /\ cacheContract.rustosAtomics

LinuxDataPlaneRequiresWbAtomics ==
    linuxReady =>
        /\ installed
        /\ cacheContract.linux = "wb"
        /\ cacheContract.linuxAtomics

KernelProducerTracksOnlyItsOwnPublishes ==
    txProducer = Cardinality(txPublished)

KernelConsumerTracksOnlyDeliveredRx ==
    kernelRxConsumer = Cardinality(deliveredRx)

OnlyValidatedRxSlotsReachNetworkPolicy ==
    deliveredRx \subseteq validRx

NoDvmControlledHeaderMutationRevokesInstalledBounds ==
    rawHeader = TamperedHeader /\ installed => installedHeader = ValidHeader

=============================================================================
