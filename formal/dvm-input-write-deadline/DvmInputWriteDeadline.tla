------------------------ MODULE DvmInputWriteDeadline ------------------------
EXTENDS Naturals, Sequences, FiniteSets

(*******************************************************************************
Models the bounded host-side FIFO used to relay fixed RDI2 input frames to
QEMU's private Unix socket.

Concrete owner and source anchor:
  * libs/driver-domain-host/src/lib.rs
    UnixInputSink::{queue_input_frame,flush_pending_nonblocking,
                    finish_input_relay,wait_for_receiver_ready}

Each member of NormalFrames or CleanupFrames is a distinct invocation token,
not an input-event value.  Real key repeats may therefore have equal payloads
but must receive different invocation tokens; the model uses token identity to
ask whether an admitted write was duplicated, reordered, or fabricated.

The model deliberately abstracts QEMU scheduling and byte contents.  The
protocol model and Kani harnesses cover those.  This model establishes the
host FIFO contract: normal traffic cannot spend the cleanup reserve, partial
writes retain their exact FIFO head, and successful close is possible only
after all admitted frames have reached the socket in admission order.
*******************************************************************************)

CONSTANTS NormalFrames, CleanupFrames, FrameBytes, MaxRetries,
          MaxBufferedFrames, CleanupReserve

Frames == NormalFrames \cup CleanupFrames
NoFrame == "none"

VARIABLES state, receiverReady, queue, frame, offset, retriesLeft,
          accepted, emitted

vars == <<state, receiverReady, queue, frame, offset, retriesLeft,
          accepted, emitted>>

Contains(sequence, value) ==
    \E index \in 1..Len(sequence): sequence[index] = value

AllUnique(sequence) ==
    \A left, right \in 1..Len(sequence):
        left # right => sequence[left] # sequence[right]

\* The in-flight frame remains resident until its final byte is accepted.  This
\* matches the concrete PendingInputFrame { bytes, offset } representation.
Resident ==
    IF state \in {"writing", "failed"}
        THEN <<frame>> \o queue
        ELSE queue

NormalResidentCount ==
    Cardinality({token \in NormalFrames : Contains(Resident, token)})

Init ==
    /\ state = "idle"
    /\ receiverReady = FALSE
    /\ queue = <<>>
    /\ frame = NoFrame
    /\ offset = 0
    /\ retriesLeft = 0
    /\ accepted = <<>>
    /\ emitted = <<>>

\* RustOS sends the exact reverse-direction RDRY token only after the bounded
\* COM2 ingress drain is active.  No outbound invocation may precede it.
ReceiverReady ==
    /\ state = "idle"
    /\ receiverReady = FALSE
    /\ receiverReady' = TRUE
    /\ UNCHANGED <<state, queue, frame, offset, retriesLeft, accepted, emitted>>

\* Normal traffic must leave CleanupReserve frames unspent even if no socket
\* write can currently progress.
AdmitNormal(token) ==
    /\ state \in {"idle", "writing"}
    /\ receiverReady
    /\ token \in NormalFrames
    /\ ~Contains(accepted, token)
    /\ Len(Resident) < MaxBufferedFrames
    /\ NormalResidentCount < MaxBufferedFrames - CleanupReserve
    /\ queue' = Append(queue, token)
    /\ accepted' = Append(accepted, token)
    /\ UNCHANGED <<state, receiverReady, frame, offset, retriesLeft, emitted>>

\* Cleanup frames (session-end and key/button releases in the concrete code)
\* may use the reserved tail capacity, but never exceed the total FIFO bound.
AdmitCleanup(token) ==
    /\ state \in {"idle", "writing"}
    /\ receiverReady
    /\ token \in CleanupFrames
    /\ ~Contains(accepted, token)
    /\ Len(Resident) < MaxBufferedFrames
    /\ queue' = Append(queue, token)
    /\ accepted' = Append(accepted, token)
    /\ UNCHANGED <<state, receiverReady, frame, offset, retriesLeft, emitted>>

StartWrite ==
    /\ state = "idle"
    /\ Len(queue) > 0
    /\ state' = "writing"
    /\ frame' = Head(queue)
    /\ queue' = Tail(queue)
    /\ offset' = 0
    /\ retriesLeft' = MaxRetries
    /\ UNCHANGED <<receiverReady, accepted, emitted>>

\* A successful OS write accepts a nonempty suffix of exactly the FIFO head.
\* Only acceptance of the final byte advances that token to emitted.
Write(chunkBytes) ==
    /\ state = "writing"
    /\ chunkBytes \in 1..(FrameBytes - offset)
    /\ IF offset + chunkBytes = FrameBytes
          THEN /\ state' = "idle"
               /\ frame' = NoFrame
               /\ offset' = 0
               /\ retriesLeft' = 0
               /\ emitted' = Append(emitted, frame)
          ELSE /\ state' = "writing"
               /\ frame' = frame
               /\ offset' = offset + chunkBytes
               /\ retriesLeft' = retriesLeft
               /\ UNCHANGED emitted
    /\ UNCHANGED <<receiverReady, queue, accepted>>

\* Nonblocking WouldBlock consumes only a bounded retry opportunity.  It may
\* neither advance the byte offset nor remove/reorder the FIFO head.
Backpressure ==
    /\ state = "writing"
    /\ retriesLeft > 0
    /\ retriesLeft' = retriesLeft - 1
    /\ UNCHANGED <<state, receiverReady, queue, frame, offset, accepted, emitted>>

\* The wall-clock queue-drain deadline yields an explicit failed relay.  The
\* resident head and tail remain accounted for; no partial frame is promoted.
ExpireWriteDeadline ==
    /\ state = "writing"
    /\ state' = "failed"
    /\ UNCHANGED <<receiverReady, queue, frame, offset, retriesLeft, accepted, emitted>>

\* finish_input_relay may also expire while all remaining work is still queued
\* behind a non-writable socket.  Preserve its FIFO accounting on this path.
ExpireQueuedDrain ==
    /\ state = "idle"
    /\ Len(queue) > 0
    /\ state' = "failed"
    /\ frame' = Head(queue)
    /\ queue' = Tail(queue)
    /\ offset' = 0
    /\ retriesLeft' = 0
    /\ UNCHANGED <<receiverReady, accepted, emitted>>

\* A successful relay return is legal only after the FIFO has fully drained.
CloseCleanly ==
    /\ state = "idle"
    /\ Len(queue) = 0
    /\ receiverReady
    /\ state' = "closed"
    /\ UNCHANGED <<receiverReady, queue, frame, offset, retriesLeft, accepted, emitted>>

Next ==
    \/ ReceiverReady
    \/ \E token \in NormalFrames: AdmitNormal(token)
    \/ \E token \in CleanupFrames: AdmitCleanup(token)
    \/ StartWrite
    \/ \E chunkBytes \in 1..FrameBytes: Write(chunkBytes)
    \/ Backpressure
    \/ ExpireWriteDeadline
    \/ ExpireQueuedDrain
    \/ CloseCleanly

TypeOK ==
    /\ state \in {"idle", "writing", "failed", "closed"}
    /\ receiverReady \in BOOLEAN
    /\ queue \in Seq(Frames)
    /\ frame \in Frames \cup {NoFrame}
    /\ offset \in 0..FrameBytes
    /\ retriesLeft \in 0..MaxRetries
    /\ accepted \in Seq(Frames)
    /\ emitted \in Seq(Frames)

ModelBoundsAreSane ==
    /\ NormalFrames \cap CleanupFrames = {}
    /\ FrameBytes > 0
    /\ MaxRetries >= 0
    /\ MaxBufferedFrames > 0
    /\ CleanupReserve \in 1..MaxBufferedFrames

AcceptedAccountingIsExact == accepted = emitted \o Resident

NoFrameBeforeRustosReceiverReady ==
    Len(accepted) > 0 => receiverReady

FifoIsBounded == Len(Resident) <= MaxBufferedFrames

NormalTrafficPreservesCleanupReserve ==
    NormalResidentCount <= MaxBufferedFrames - CleanupReserve

AdmittedFramesAreNeverDuplicated ==
    /\ AllUnique(accepted)
    /\ AllUnique(emitted)

PartialWriteKeepsItsHeadUncommitted ==
    state = "writing" =>
        /\ offset < FrameBytes
        /\ ~Contains(emitted, frame)

FailedRelayIsExplicitAndUncommitted ==
    state = "failed" =>
        /\ Len(Resident) > 0
        /\ accepted # emitted

CleanCloseDrainsExactly ==
    state = "closed" =>
        /\ Len(Resident) = 0
        /\ accepted = emitted

Spec == Init /\ [][Next]_vars
=============================================================================
