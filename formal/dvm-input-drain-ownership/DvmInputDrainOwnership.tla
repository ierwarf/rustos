----------------------- MODULE DvmInputDrainOwnership -----------------------
EXTENDS Naturals

(*******************************************************************************
Models ownership of the COM2 RDI2 decoder.

Concrete owner and source anchor:
  * kernel/io-manager/src/input/dvm_serial.rs service_pending
  * kernel/compat/src/user/syscall/linux/input_broker_ops.rs
  * kernel/compat/src/user/syscall/linux/service_ops/poll_epoll.rs

The scheduler/RTC path may wake normal work, but it must never invoke the
decoder or mutate its frame state.  The only decoder owner is the
capability-gated broker call in task context.  This excludes the former
failure mode where an RTC interrupt preempted a broker-owned Decoder lock and
recursively acquired it from the same task identity.

The model abstracts byte validation and input policy. DvmInputRevocation
covers frame provenance and reset barriers; this model covers the narrower
execution-context and bounded-ingress contract.
*******************************************************************************)

CONSTANTS RawCapacity, IngressCapacity

NoOwner == "none"
BrokerOwner == "broker"

VARIABLES receiverReady, streamRequested, decoderOwner, rawFrames,
          ingressFrames, tickWakePending

vars == <<receiverReady, streamRequested, decoderOwner, rawFrames,
          ingressFrames, tickWakePending>>

Init ==
    /\ receiverReady = FALSE
    /\ streamRequested = FALSE
    /\ decoderOwner = NoOwner
    /\ rawFrames = 0
    /\ ingressFrames = 0
    /\ tickWakePending = FALSE

\* The first broker drain attempt emits RDRY. L0 cannot request DVM input
\* before this task-context ownership boundary is live.
AnnounceReceiverReady ==
    /\ receiverReady = FALSE
    /\ decoderOwner = NoOwner
    /\ receiverReady' = TRUE
    /\ UNCHANGED <<streamRequested, decoderOwner, rawFrames, ingressFrames,
                  tickWakePending>>

RequestDvmStream ==
    /\ receiverReady
    /\ streamRequested = FALSE
    /\ streamRequested' = TRUE
    /\ UNCHANGED <<receiverReady, decoderOwner, rawFrames, ingressFrames,
                  tickWakePending>>

\* Hardware may have a finite number of complete frames pending. This is a
\* transport fact, not an authorization or a decoder invocation.
FrameArrives ==
    /\ streamRequested
    /\ rawFrames < RawCapacity
    /\ rawFrames' = rawFrames + 1
    /\ UNCHANGED <<receiverReady, streamRequested, decoderOwner, ingressFrames,
                  tickWakePending>>

\* An RTC scheduling tick may request prompt task scheduling, but it has no
\* decoder authority and cannot change raw or decoded input state.
RtcTick ==
    /\ tickWakePending' = TRUE
    /\ UNCHANGED <<receiverReady, streamRequested, decoderOwner, rawFrames,
                  ingressFrames>>

BeginBrokerDrain ==
    /\ receiverReady
    /\ decoderOwner = NoOwner
    /\ rawFrames > 0
    /\ decoderOwner' = BrokerOwner
    /\ UNCHANGED <<receiverReady, streamRequested, rawFrames, ingressFrames,
                  tickWakePending>>

\* Only the broker-owned decoder may transfer one validated frame into the
\* fixed ingress queue. A full ingress queue leaves the raw frame pending.
DrainOneFrame ==
    /\ decoderOwner = BrokerOwner
    /\ rawFrames > 0
    /\ ingressFrames < IngressCapacity
    /\ rawFrames' = rawFrames - 1
    /\ ingressFrames' = ingressFrames + 1
    /\ UNCHANGED <<receiverReady, streamRequested, decoderOwner,
                  tickWakePending>>

FinishBrokerDrain ==
    /\ decoderOwner = BrokerOwner
    /\ decoderOwner' = NoOwner
    /\ tickWakePending' = FALSE
    /\ UNCHANGED <<receiverReady, streamRequested, rawFrames, ingressFrames>>

ConsumeIngress ==
    /\ ingressFrames > 0
    /\ decoderOwner = NoOwner
    /\ ingressFrames' = ingressFrames - 1
    /\ UNCHANGED <<receiverReady, streamRequested, decoderOwner, rawFrames,
                  tickWakePending>>

Next ==
    \/ AnnounceReceiverReady
    \/ RequestDvmStream
    \/ FrameArrives
    \/ RtcTick
    \/ BeginBrokerDrain
    \/ DrainOneFrame
    \/ FinishBrokerDrain
    \/ ConsumeIngress

TypeOK ==
    /\ receiverReady \in BOOLEAN
    /\ streamRequested \in BOOLEAN
    /\ decoderOwner \in {NoOwner, BrokerOwner}
    /\ rawFrames \in 0..RawCapacity
    /\ ingressFrames \in 0..IngressCapacity
    /\ tickWakePending \in BOOLEAN

NoIrqDecoderOwnership == decoderOwner # "rtc"

NoRecursiveDecoderOwnership == decoderOwner \in {NoOwner, BrokerOwner}

StreamStartsOnlyAfterTaskContextReceiverReady ==
    streamRequested => receiverReady

IngressIsBounded == ingressFrames <= IngressCapacity

RawTransportIsBounded == rawFrames <= RawCapacity

TickCannotMutateDecoderOrIngress ==
    tickWakePending => decoderOwner \in {NoOwner, BrokerOwner}

Spec == Init /\ [][Next]_vars
=============================================================================
