-------------------------- MODULE DvmInputRevocation --------------------------
EXTENDS Naturals, Sequences, FiniteSets

(*******************************************************************************
Models DVM input-session revocation.

Concrete owners and source anchors:
  * RDI3 epoch replacement: kernel/io-manager/src/input/dvm_frames.rs
  * priority reset queue barrier: kernel/io-manager/src/input/event_queue.rs
  * reset-wire admission and synthetic key releases: services/inputd/src/main.rs
  * provider key-state reset: drivers/libs/keyboard-core/src/lib.rs

The model abstracts frame CRC/serial parsing and mouse motion. A queued key
stands for an already-validated RDI3 key transition. The queue has a small
capacity; its important property is that a reset replaces every older queued
frame rather than waiting behind a hostile producer.
*******************************************************************************)

CONSTANTS Epochs, Keys, MaxQueue

NoEpoch == 0
NoKey == "none"
Reset(epoch) == [kind |-> "reset", epoch |-> epoch, key |-> NoKey, pressed |-> FALSE]
KeyTransition(epoch, key, pressed) ==
    [kind |-> "key", epoch |-> epoch, key |-> key, pressed |-> pressed]

VARIABLES currentEpoch, resetDeliveredEpoch, issuedEpochs, queue, heldKeys, heldEpoch

vars == <<currentEpoch, resetDeliveredEpoch, issuedEpochs, queue, heldKeys, heldEpoch>>

QueueHasReset == \E index \in 1..Len(queue): queue[index].kind = "reset"

Init ==
    /\ currentEpoch = NoEpoch
    /\ resetDeliveredEpoch = NoEpoch
    /\ issuedEpochs = {}
    /\ queue = <<>>
    /\ heldKeys = {}
    /\ heldEpoch = NoEpoch

\* A RDI3 SESSION_START always inserts a revocation barrier, even when the
\* preceding DVM died without SESSION_END. This is the queue replacement in
\* event_queue::submit_dvm_input_reset.
StartSession(epoch) ==
    /\ epoch \in Epochs
    \* L0 allocates a fresh epoch for every authenticated relay.  Reusing a
    \* retired epoch would let a recorded sequence restart at one after a
    \* later reset barrier, turning the sequence check into a replay oracle.
    /\ epoch \notin issuedEpochs
    /\ currentEpoch' = epoch
    /\ issuedEpochs' = issuedEpochs \cup {epoch}
    /\ queue' = <<Reset(epoch)>>
    /\ UNCHANGED <<resetDeliveredEpoch, heldKeys, heldEpoch>>

\* A validated transition may queue after the reset barrier, but cannot be
\* delivered until that barrier has reached inputd.
QueueKey(key, pressed) ==
    /\ currentEpoch \in Epochs
    /\ key \in Keys
    /\ pressed \in BOOLEAN
    /\ Len(queue) < MaxQueue
    /\ queue' = Append(queue, KeyTransition(currentEpoch, key, pressed))
    /\ UNCHANGED <<currentEpoch, resetDeliveredEpoch, issuedEpochs, heldKeys, heldEpoch>>

\* SESSION_END uses the same priority reset. The old producer loses its
\* current epoch immediately; inputd emits synthetic releases on delivery.
EndSession ==
    /\ currentEpoch \in Epochs
    /\ currentEpoch' = NoEpoch
    /\ queue' = <<Reset(currentEpoch)>>
    /\ UNCHANGED <<resetDeliveredEpoch, issuedEpochs, heldKeys, heldEpoch>>

DeliverReset ==
    /\ Len(queue) > 0
    /\ queue[1].kind = "reset"
    /\ queue' = SubSeq(queue, 2, Len(queue))
    /\ resetDeliveredEpoch' = queue[1].epoch
    /\ heldKeys' = {}
    /\ heldEpoch' = NoEpoch
    /\ UNCHANGED <<currentEpoch, issuedEpochs>>

DeliverKey ==
    /\ Len(queue) > 0
    /\ queue[1].kind = "key"
    /\ currentEpoch \in Epochs
    /\ queue[1].epoch = currentEpoch
    /\ resetDeliveredEpoch = currentEpoch
    /\ queue' = SubSeq(queue, 2, Len(queue))
    /\ heldKeys' = IF queue[1].pressed
                      THEN heldKeys \cup {queue[1].key}
                      ELSE heldKeys \ {queue[1].key}
    /\ heldEpoch' = IF queue[1].pressed THEN currentEpoch
                    ELSE IF heldKeys \ {queue[1].key} = {} THEN NoEpoch ELSE currentEpoch
    /\ UNCHANGED <<currentEpoch, resetDeliveredEpoch, issuedEpochs>>

Next ==
    \/ \E epoch \in Epochs: StartSession(epoch)
    \/ \E key \in Keys, pressed \in BOOLEAN: QueueKey(key, pressed)
    \/ EndSession
    \/ DeliverReset
    \/ DeliverKey

TypeOK ==
    /\ currentEpoch \in Epochs \cup {NoEpoch}
    /\ resetDeliveredEpoch \in Epochs \cup {NoEpoch}
    /\ issuedEpochs \subseteq Epochs
    /\ queue \in Seq({Reset(epoch) : epoch \in Epochs}
                    \cup {KeyTransition(epoch, key, pressed) :
                              epoch \in Epochs, key \in Keys, pressed \in BOOLEAN})
    /\ Len(queue) <= MaxQueue
    /\ heldKeys \subseteq Keys
    /\ heldEpoch \in Epochs \cup {NoEpoch}

ResetBarrierLeadsTheQueue ==
    QueueHasReset => queue[1].kind = "reset"

QueuedKeysBelongToTheCurrentDvmEpoch ==
    \A index \in 1..Len(queue):
        queue[index].kind = "key" => queue[index].epoch = currentEpoch

NoKeyPassesBeforeItsEpochReset ==
    \A index \in 1..Len(queue):
        queue[index].kind = "key" /\ resetDeliveredEpoch # queue[index].epoch => index > 1

DeliveredCurrentEpochOwnsEveryHeldKey ==
    currentEpoch \in Epochs /\ resetDeliveredEpoch = currentEpoch =>
        /\ heldKeys = {} \/ heldEpoch = currentEpoch
        /\ heldEpoch \in {NoEpoch, currentEpoch}

NoActiveSessionAfterEndCanReceiveInput ==
    currentEpoch = NoEpoch =>
        \A index \in 1..Len(queue): queue[index].kind # "key"

ActiveEpochWasIssuedExactlyOnce ==
    currentEpoch \in Epochs => currentEpoch \in issuedEpochs

Spec == Init /\ [][Next]_vars
=============================================================================
